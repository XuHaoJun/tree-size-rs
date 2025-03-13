mod platform;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;
use dashmap::{DashMap, DashSet};
use serde::Serialize;
use kanal;
use tauri::Emitter;

/// Contains analytics information for a directory or file
#[derive(Debug)]
struct AnalyticsInfo {
    /// Total size in bytes
    size_bytes: AtomicU64,
    /// Total number of entries (files and directories)
    entry_count: AtomicU64,
    /// Number of files
    file_count: AtomicU64,
    /// Number of directories
    directory_count: AtomicU64,
}

impl AnalyticsInfo {
    fn new_with_size(size: u64, is_dir: bool) -> Self {
        let file_count = if is_dir { 0 } else { 1 };
        let directory_count = if is_dir { 1 } else { 0 };
        
        Self {
            size_bytes: AtomicU64::new(size),
            entry_count: AtomicU64::new(1), // Count itself as 1 entry
            file_count: AtomicU64::new(file_count),
            directory_count: AtomicU64::new(directory_count),
        }
    }
}

/// Represents size information for a file or directory
#[derive(Clone, Debug, Serialize)]
struct FileSystemEntry {
    /// Path to the file or directory
    path: PathBuf,
    /// Size in bytes
    size_bytes: u64,
    /// Number of entries (files and directories)
    entry_count: u64,
    /// Number of files
    file_count: u64,
    /// Number of directories
    directory_count: u64,
}

// Asynchronous recursive processing function
async fn calculate_size_async(
    path: PathBuf,
    analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
    sender: Option<kanal::Sender<FileSystemEntry>>,
    target_dir_path: Option<PathBuf>,
    visited_inodes: Option<Arc<DashSet<(u64, u64)>>>, // Track device and inode pairs
) -> std::io::Result<()> {
    // Check if path is a symlink
    let is_symlink = path.is_symlink();
    
    if is_symlink {
        // Only count the symlink itself, not what it points to
        if let Some(path_info) = platform::get_path_info(&path, true, false) {
            update_analytics(&path, path_info.size, 1, 0, 0, &analytics_map, &sender);
        }
        return Ok(());
    }

    // Get path info using our platform-agnostic function
    let path_info = match platform::get_path_info(&path, true, true) {
        Some(info) => info,
        None => return Ok(()),
    };
    
    // Check for cycles using device and inode numbers if available
    if let Some(inode_pair) = path_info.inode_device {
        let visited = visited_inodes.clone().unwrap_or_else(|| Arc::new(DashSet::new()));
        
        // If we've seen this inode before, we have a cycle
        if !visited.insert(inode_pair) && path_info.is_dir {
            return Ok(());
        }
    }

    let entry_count = 1; // Count this file/directory as 1
    let file_count = if path_info.is_dir { 0 } else { 1 };
    let directory_count = if path_info.is_dir { 1 } else { 0 };

    // Handle updates directly
    let mut updates = vec![];
    if path_info.is_dir {
        match analytics_map.entry(path.clone()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                updates.push((e.get().clone(), path_info.size, entry_count, file_count, directory_count, path.clone()));
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let analytics = Arc::new(AnalyticsInfo::new_with_size(path_info.size, path_info.is_dir));
                updates.push((analytics.clone(), 0, 0, 0, 0, path.clone()));
                e.insert(analytics);
            }
        }
    }
    // Skip parent size calculation if this is the target directory
    let is_target_dir = match &target_dir_path {
        Some(target) => path == *target,
        None => false,
    };

    if !is_target_dir {
        // Update parent directories
        let parent_chain = get_parent_chain_sync(&path);
        for parent in parent_chain {
            // Skip if parent is not in map
            if let Some(analytics) = analytics_map.get(&parent) {
                analytics.size_bytes.fetch_add(path_info.size, Ordering::Relaxed);
                analytics.entry_count.fetch_add(entry_count, Ordering::Relaxed);
                analytics.file_count.fetch_add(file_count, Ordering::Relaxed);
                analytics.directory_count.fetch_add(directory_count, Ordering::Relaxed);
                
                // Send parent path and values to channel if available
                if let Some(sender) = &sender {
                    let _ = sender.send(FileSystemEntry {
                        path: parent.clone(),
                        size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
                        entry_count: analytics.entry_count.load(Ordering::Relaxed),
                        file_count: analytics.file_count.load(Ordering::Relaxed),
                        directory_count: analytics.directory_count.load(Ordering::Relaxed),
                    });
                }
            }
        }
    }

    // Process updates
    for (analytics, size, count, file_count, directory_count, path) in updates {
        analytics.size_bytes.fetch_add(size, Ordering::Relaxed);
        analytics.entry_count.fetch_add(count, Ordering::Relaxed);
        analytics.file_count.fetch_add(file_count, Ordering::Relaxed);
        analytics.directory_count.fetch_add(directory_count, Ordering::Relaxed);
        
        // Send path and values to channel if available
        if let Some(sender) = &sender {
            let _ = sender.send(FileSystemEntry {
                path: path,
                size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
                entry_count: analytics.entry_count.load(Ordering::Relaxed),
                file_count: analytics.file_count.load(Ordering::Relaxed),
                directory_count: analytics.directory_count.load(Ordering::Relaxed),
            });
        }
    }

    // Process subdirectories asynchronously
    if path_info.is_dir {
        let mut entries = Vec::new();
        let mut children = fs::read_dir(&path).await?;

        // Create visited_inodes set if it doesn't exist
        let visited = visited_inodes.clone().unwrap_or_else(|| Arc::new(DashSet::new()));

        // Collect all subdirectory entries
        while let Some(entry) = children.next_entry().await? {
            entries.push(entry.path());
        }

        // Process each subdirectory sequentially but asynchronously
        for child_path in entries {
            // Use Box::pin to handle recursive async calls
            let future = Box::pin(calculate_size_async(
                child_path,
                analytics_map.clone(),
                sender.clone(),
                target_dir_path.clone(),
                Some(visited.clone()),
            ));
            future.await?;
        }
    }

    Ok(())
}

// Helper function to update analytics and send events
fn update_analytics(
    path: &Path, 
    size: u64,
    entry_count: u64,
    file_count: u64,
    directory_count: u64,
    analytics_map: &Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
    sender: &Option<kanal::Sender<FileSystemEntry>>
) {
    if let Some(analytics) = analytics_map.get(path) {
        analytics.size_bytes.fetch_add(size, Ordering::Relaxed);
        analytics.entry_count.fetch_add(entry_count, Ordering::Relaxed);
        analytics.file_count.fetch_add(file_count, Ordering::Relaxed);
        analytics.directory_count.fetch_add(directory_count, Ordering::Relaxed);
        
        // Send path and values to channel if available
        if let Some(sender) = sender {
            let _ = sender.send(FileSystemEntry {
                path: path.to_path_buf(),
                size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
                entry_count: analytics.entry_count.load(Ordering::Relaxed),
                file_count: analytics.file_count.load(Ordering::Relaxed),
                directory_count: analytics.directory_count.load(Ordering::Relaxed),
            });
        }
    }
}

// 同步版本的获取父目录链函数，用于 Rayon 并行处理
fn get_parent_chain_sync(path: &Path) -> Vec<PathBuf> {
    let mut chain = vec![];
    let mut current = path;
    while let Some(parent) = current.parent() {
        chain.push(parent.to_path_buf());
        current = parent;
    }
    chain
}

// New command to stream directory sizes to the frontend
#[tauri::command]
async fn scan_directory_size(path: String, window: tauri::Window) -> Result<(), String> {
    // Drop any previous channel/resources before starting a new scan
    // This is especially important on Windows to prevent resource leaks
    tokio::task::yield_now().await;
    
    let result = scan_directory_with_events(path, window.clone()).await;
    
    // Ensure we emit a complete event even on error to clean up frontend state
    if result.is_err() {
        // Try to emit completion event on error to ensure frontend cleans up
        let _ = window.emit("scan-complete", ());
    }
    
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

async fn scan_directory_with_events(path: String, window: tauri::Window) -> std::io::Result<()> {
    let target_dir = Path::new(&path).canonicalize()?;
    let analytics_map = Arc::new(DashMap::new());
    // Initialize the set to track visited inodes (to prevent cycles)
    let visited_inodes = Arc::new(DashSet::new());

    // Create a channel with capacity 1000 for sending file system entries
    let (sender, receiver) = kanal::bounded::<FileSystemEntry>(1000);

    // Spawn a task to process received entries and emit them as Tauri events
    let window_clone = window.clone();
    let receiver_handle = tokio::spawn(async move {
        while let Ok(entry) = receiver.recv() {
            // Emit each file/directory entry as an event to the frontend
            if let Err(e) = window_clone.emit("directory-entry", &entry) {
                eprintln!("Failed to emit event: {}", e);
            }
        }

        // Emit a completion event when done
        if let Err(e) = window_clone.emit("scan-complete", ()) {
            eprintln!("Failed to emit completion event: {}", e);
        }
    });

    // Clone sender for the calculation task
    let sender_clone = sender.clone();
    
    // Run the calculate_size_async function in a separate task
    let calc_task = tokio::spawn(calculate_size_async(
        target_dir.clone(),
        analytics_map.clone(),
        Some(sender_clone),
        Some(target_dir),
        Some(visited_inodes),
    ));

    // Wait for calculation to complete and handle any errors
    if let Err(e) = calc_task.await? {
        eprintln!("Error during directory calculation: {}", e);
        return Err(e);
    }
    
    // Explicitly drop the sender to close the channel
    drop(sender);

    // Wait for receiver task to complete
    let _ = receiver_handle.await;
    
    // Explicitly clear resources to ensure they're dropped
    // This is especially important for Windows
    drop(analytics_map);

    Ok(())
}

#[tauri::command]
fn get_free_space(path: String) -> Result<u64, String> {
    match platform::get_space_info(&path) {
        Some((_, available, _)) => Ok(available),
        None => Err("Failed to get free space".to_string()),
    }
}

#[tauri::command]
fn get_space_info(path: String) -> Result<(u64, u64, u64), String> {
    match platform::get_space_info(&path) {
        Some((total, available, used)) => Ok((total, available, used)),
        None => Err("Failed to get space information".to_string()),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Create a multi-threaded Tokio runtime
    let runtime = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    // Set the runtime as the default for this thread
    let _guard = runtime.enter();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            scan_directory_size,
            get_free_space,
            get_space_info
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_calculate_size_empty_directory() -> std::io::Result<()> {
        // Create a temporary directory for testing
        let temp_dir = tempdir()?;
        let path = temp_dir.path().to_path_buf();
        
        // Create analytics map and visited inodes
        let analytics_map = Arc::new(DashMap::new());
        let visited_inodes = Arc::new(DashSet::new());
        
        // Run the function
        calculate_size_async(
            path.clone(),
            analytics_map.clone(),
            None, // No sender needed for this test
            None, // No target directory
            Some(visited_inodes),
        ).await?;
        
        // Verify the results
        assert!(analytics_map.contains_key(&path), "Path should be in the analytics map");
        
        let analytics = analytics_map.get(&path).unwrap();
        // Directories have a non-zero size on filesystems
        assert!(analytics.size_bytes.load(Ordering::Relaxed) > 0, "Directory should have a size on the filesystem");
        assert_eq!(analytics.entry_count.load(Ordering::Relaxed), 1, "Should count itself as one entry");
        assert_eq!(analytics.file_count.load(Ordering::Relaxed), 0, "Should have 0 files");
        assert_eq!(analytics.directory_count.load(Ordering::Relaxed), 1, "Should count itself as one directory");
        
        Ok(())
    }
    
    #[tokio::test]
    async fn test_calculate_size_with_files() -> std::io::Result<()> {
        // Create a temporary directory with files
        let temp_dir = tempdir()?;
        let path = temp_dir.path().to_path_buf();
        
        // Create a file with known content
        let file_path = path.join("test_file.txt");
        let mut file = File::create(&file_path)?;
        let test_data = "Hello, world!";
        file.write_all(test_data.as_bytes())?;
        
        // Create analytics map and visited inodes
        let analytics_map = Arc::new(DashMap::new());
        let visited_inodes = Arc::new(DashSet::new());
        
        // Run the function
        calculate_size_async(
            path.clone(),
            analytics_map.clone(),
            None,
            None,
            Some(visited_inodes),
        ).await?;
        
        // Verify the results
        assert!(analytics_map.contains_key(&path), "Path should be in the analytics map");
        
        let analytics = analytics_map.get(&path).unwrap();
        assert!(analytics.size_bytes.load(Ordering::Relaxed) >= test_data.len() as u64, 
            "Directory size should include the file size");
        assert_eq!(analytics.entry_count.load(Ordering::Relaxed), 2, "Should count itself and one file");
        assert_eq!(analytics.file_count.load(Ordering::Relaxed), 1, "Should have 1 file");
        assert_eq!(analytics.directory_count.load(Ordering::Relaxed), 1, "Should count itself as one directory");
        
        Ok(())
    }
    
    #[tokio::test]
    async fn test_calculate_size_with_subdirectories() -> std::io::Result<()> {
        // Create a temporary directory with subdirectories and files
        let temp_dir = tempdir()?;
        let path = temp_dir.path().to_path_buf();
        
        // Create a subdirectory
        let subdir_path = path.join("subdir");
        fs::create_dir(&subdir_path)?;
        
        // Create a file in the subdirectory
        let file_path = subdir_path.join("test_file.txt");
        let mut file = File::create(&file_path)?;
        let test_data = "Hello, nested world!";
        file.write_all(test_data.as_bytes())?;
        
        // Create another file in the root directory
        let root_file_path = path.join("root_file.txt");
        let mut root_file = File::create(&root_file_path)?;
        let root_test_data = "Root data";
        root_file.write_all(root_test_data.as_bytes())?;
        
        // Create analytics map and visited inodes
        let analytics_map = Arc::new(DashMap::new());
        let visited_inodes = Arc::new(DashSet::new());
        
        // Run the function
        calculate_size_async(
            path.clone(),
            analytics_map.clone(),
            None,
            None,
            Some(visited_inodes),
        ).await?;
        
        // Verify the results for the root directory
        assert!(analytics_map.contains_key(&path), "Root path should be in the analytics map");
        
        let root_analytics = analytics_map.get(&path).unwrap();
        let expected_size = (test_data.len() + root_test_data.len()) as u64;
        assert!(root_analytics.size_bytes.load(Ordering::Relaxed) >= expected_size, 
            "Directory size should include all files and subdirectories");
        assert_eq!(root_analytics.entry_count.load(Ordering::Relaxed), 4, 
            "Should count root dir, subdir, and 2 files");
        assert_eq!(root_analytics.file_count.load(Ordering::Relaxed), 2, "Should have 2 files");
        assert_eq!(root_analytics.directory_count.load(Ordering::Relaxed), 2, 
            "Should count root and subdirectory");
        
        // Verify the results for the subdirectory
        assert!(analytics_map.contains_key(&subdir_path), "Subdir path should be in the analytics map");
        
        let subdir_analytics = analytics_map.get(&subdir_path).unwrap();
        assert!(subdir_analytics.size_bytes.load(Ordering::Relaxed) >= test_data.len() as u64, 
            "Subdirectory size should include its file size");
        assert_eq!(subdir_analytics.entry_count.load(Ordering::Relaxed), 2, 
            "Should count subdir and its file");
        assert_eq!(subdir_analytics.file_count.load(Ordering::Relaxed), 1, "Should have 1 file");
        assert_eq!(subdir_analytics.directory_count.load(Ordering::Relaxed), 1, 
            "Should count itself as one directory");
        
        Ok(())
    }
    
    #[tokio::test]
    #[cfg_attr(target_os = "windows", ignore)]
    async fn test_calculate_size_with_symlink() -> std::io::Result<()> {
        // Skip this test on Windows as symlink creation requires admin privileges
        if cfg!(windows) {
            return Ok(());
        }
        
        // Create a temporary directory with a symlink
        let temp_dir = tempdir()?;
        let path = temp_dir.path().to_path_buf();
        
        // Create a file with known content
        let file_path = path.join("test_file.txt");
        let mut file = File::create(&file_path)?;
        let test_data = "Hello, world!";
        file.write_all(test_data.as_bytes())?;
        
        // Create a symlink to the file
        let symlink_path = path.join("test_symlink");
        std::os::unix::fs::symlink(&file_path, &symlink_path)?;
        
        // Create analytics map and visited inodes
        let analytics_map = Arc::new(DashMap::new());
        let visited_inodes = Arc::new(DashSet::new());
        
        // Run the function
        calculate_size_async(
            path.clone(),
            analytics_map.clone(),
            None,
            None,
            Some(visited_inodes),
        ).await?;
        
        // Verify the results
        assert!(analytics_map.contains_key(&path), "Path should be in the analytics map");
        
        let analytics = analytics_map.get(&path).unwrap();
        // The symlink is counted in the entry count, but not as a file
        // In some implementations, symlinks might be treated differently
        assert!(analytics.entry_count.load(Ordering::Relaxed) >= 2, 
            "Should count at least the directory and file");
        assert_eq!(analytics.file_count.load(Ordering::Relaxed), 1, 
            "Should count only the actual file, not the symlink as a file");
        
        // Let's also check that the symlink exists
        assert!(symlink_path.exists(), "Symlink should exist");
        
        Ok(())
    }
    
    #[tokio::test]
    async fn test_calculate_size_with_events() -> std::io::Result<()> {
        // Create a temporary directory with files
        let temp_dir = tempdir()?;
        let path = temp_dir.path().to_path_buf();
        
        // Create a file with known content
        let file_path = path.join("test_file.txt");
        let mut file = File::create(&file_path)?;
        let test_data = "Hello, world!";
        file.write_all(test_data.as_bytes())?;
        
        // Create a channel to receive events
        let (sender, receiver) = kanal::bounded::<FileSystemEntry>(100);
        
        // Create analytics map and visited inodes
        let analytics_map = Arc::new(DashMap::new());
        let visited_inodes = Arc::new(DashSet::new());
        
        // Run the function
        calculate_size_async(
            path.clone(),
            analytics_map.clone(),
            Some(sender),
            None,
            Some(visited_inodes),
        ).await?;
        
        // Drop sender to close the channel
        drop(analytics_map);
        
        // Count received events
        let mut entries_received = 0;
        while let Ok(_) = receiver.recv() {
            entries_received += 1;
        }
        
        // Verify we received events
        assert!(entries_received > 0, "Should have received events for paths");
        
        Ok(())
    }
}
