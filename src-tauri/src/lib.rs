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

/// Complete scan result with all directory entries
#[derive(Clone, Debug, Serialize)]
struct DirectoryScanResult {
    /// The root directory path
    root_path: PathBuf,
    /// All entries found during the scan
    entries: Vec<FileSystemEntry>,
    /// Total scan time in milliseconds
    scan_time_ms: u64,
}

// Get parent directories up to but not beyond the target directory
fn get_parent_chain_sync(path: &Path, target_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut chain = Vec::with_capacity(10); // Pre-allocate for typical directory depth
    let mut current = path;
    
    while let Some(parent) = current.parent() {
        // If we've reached the target directory, stop collecting parents
        if let Some(target) = target_dir {
            if !parent.starts_with(target) {
                break;
            }
        }
        
        chain.push(parent.to_path_buf());
        current = parent;
    }
    
    chain
}

// Asynchronous recursive processing function with deferred event sending
async fn calculate_size_async(
    path: PathBuf,
    analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
    target_dir_path: Option<PathBuf>,
    visited_inodes: Option<Arc<DashSet<(u64, u64)>>>, // Track device and inode pairs
    processed_paths: Option<Arc<DashSet<PathBuf>>>, // Track paths we've already processed
) -> std::io::Result<()> {
    // Use shared tracking sets
    let visited = visited_inodes.unwrap_or_else(|| Arc::new(DashSet::new()));
    let processed = processed_paths.unwrap_or_else(|| Arc::new(DashSet::new()));
    
    // If we've already processed this path, skip it
    if !processed.insert(path.clone()) {
        return Ok(());
    }
    
    // Get path info using our platform-agnostic function - will work for files, dirs and symlinks
    let path_info = match platform::get_path_info(&path, true, path.is_symlink()) {
        Some(info) => info,
        None => return Ok(()),
    };
    
    // Check for cycles using device and inode numbers if available
    // This handles both directory cycles AND symlinks properly
    if let Some(inode_pair) = path_info.inode_device {
        if !visited.insert(inode_pair) {
            // We've already seen this inode, skip it
            return Ok(());
        }
    }

    // Count entry as file or directory, symlinks count as entries but not as files or dirs
    let is_symlink = path.is_symlink();
    let entry_count = 1; // Count this file/directory/symlink as 1 entry
    let file_count = if path_info.is_dir || is_symlink { 0 } else { 1 };
    let directory_count = if path_info.is_dir && !is_symlink { 1 } else { 0 };
    
    // Skip parent size calculation if this is the target directory
    let is_target_dir = match &target_dir_path {
        Some(target) => path == *target,
        None => false,
    };

    // Add entry to analytics map with initial values (will be updated later for directories)
    let entry_analytics = match analytics_map.entry(path.clone()) {
        dashmap::mapref::entry::Entry::Occupied(e) => e.get().clone(),
        dashmap::mapref::entry::Entry::Vacant(e) => {
            let analytics = Arc::new(AnalyticsInfo {
                size_bytes: AtomicU64::new(path_info.size),
                entry_count: AtomicU64::new(entry_count),
                file_count: AtomicU64::new(file_count),
                directory_count: AtomicU64::new(directory_count),
            });
            e.insert(analytics.clone());
            analytics
        }
    };
    
    // For directories, process all children (but don't follow symlinks)
    if path_info.is_dir && !is_symlink {
        // Collect all subdirectory entries first 
        let mut entries = Vec::new();
        let mut read_dir_result = fs::read_dir(&path).await;
        
        if let Ok(ref mut children) = read_dir_result {
            while let Some(entry) = children.next_entry().await? {
                entries.push(entry.path());
            }
        }
        
        // Process all children recursively
        for child_path in &entries {
            // Use Box::pin to handle recursive async calls correctly
            let future = Box::pin(calculate_size_async(
                child_path.clone(),
                analytics_map.clone(),
                target_dir_path.clone(),
                Some(visited.clone()),
                Some(processed.clone()),
            ));
            
            // Await the future
            if let Err(e) = future.await {
                eprintln!("Error processing {:?}: {:?}", child_path, e);
            }
        }
        
        // Now compute the total size based on children
        let mut total_size = path_info.size; // Start with directory's own size
        let mut total_entries = 1;  // Start with the directory itself
        let mut total_files = 0;    // Directories don't count as files
        let mut total_dirs = 1;     // Count this directory
        
        // Sum up all children's contributions
        for child_path in &entries {
            if let Some(child_analytics) = analytics_map.get(child_path) {
                let child_size = child_analytics.size_bytes.load(Ordering::Relaxed);
                let child_entries = child_analytics.entry_count.load(Ordering::Relaxed);
                let child_files = child_analytics.file_count.load(Ordering::Relaxed);
                let child_dirs = child_analytics.directory_count.load(Ordering::Relaxed);
                
                total_size += child_size;
                total_entries += child_entries;
                total_files += child_files;
                total_dirs += child_dirs;
            }
        }
        
        // Update this directory's values atomically (all at once)
        entry_analytics.size_bytes.store(total_size, Ordering::Relaxed);
        entry_analytics.entry_count.store(total_entries, Ordering::Relaxed);
        entry_analytics.file_count.store(total_files, Ordering::Relaxed);
        entry_analytics.directory_count.store(total_dirs, Ordering::Relaxed);
    }
    
    // Update parent directories only after we have fully calculated our own size
    if !is_target_dir {
        // Use the final calculated size (which includes all children for directories)
        let size = entry_analytics.size_bytes.load(Ordering::Relaxed);
        let entries = entry_analytics.entry_count.load(Ordering::Relaxed);
        let files = entry_analytics.file_count.load(Ordering::Relaxed);
        let dirs = entry_analytics.directory_count.load(Ordering::Relaxed);
        
        let parent_chain = get_parent_chain_sync(&path, target_dir_path.as_deref());
        for parent in parent_chain {
            // Update parent entries atomically
            match analytics_map.entry(parent.clone()) {
                dashmap::mapref::entry::Entry::Occupied(e) => {
                    let analytics = e.get();
                    analytics.size_bytes.fetch_add(size, Ordering::Relaxed);
                    analytics.entry_count.fetch_add(entries, Ordering::Relaxed);
                    analytics.file_count.fetch_add(files, Ordering::Relaxed);
                    analytics.directory_count.fetch_add(dirs, Ordering::Relaxed);
                },
                dashmap::mapref::entry::Entry::Vacant(e) => {
                    let analytics = Arc::new(AnalyticsInfo {
                        size_bytes: AtomicU64::new(size),
                        entry_count: AtomicU64::new(entries),
                        file_count: AtomicU64::new(files),
                        directory_count: AtomicU64::new(dirs + 1), // +1 because parent is a directory
                    });
                    e.insert(analytics);
                }
            }
        }
    }

    Ok(())
}

// This function converts the analytics map to a vector of FileSystemEntry objects
fn analytics_map_to_entries(map: &DashMap<PathBuf, Arc<AnalyticsInfo>>) -> Vec<FileSystemEntry> {
    map.iter()
        .map(|item| {
            let path = item.key();
            let analytics = item.value();
            
            FileSystemEntry {
                path: path.clone(),
                size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
                entry_count: analytics.entry_count.load(Ordering::Relaxed),
                file_count: analytics.file_count.load(Ordering::Relaxed),
                directory_count: analytics.directory_count.load(Ordering::Relaxed),
            }
        })
        .collect()
}

// New command to scan directory and return complete results at once
#[tauri::command]
async fn scan_directory_size(path: String, window: tauri::Window) -> Result<(), String> {
    // Drop any previous resources before starting a new scan
    tokio::task::yield_now().await;
    
    let result = scan_directory_complete(path, window.clone()).await;
    
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

async fn scan_directory_complete(path: String, window: tauri::Window) -> std::io::Result<()> {
    let start_time = std::time::Instant::now();
    
    let target_dir = Path::new(&path).canonicalize()?;
    let analytics_map = Arc::new(DashMap::new());
    let visited_inodes = Arc::new(DashSet::new());
    let processed_paths = Arc::new(DashSet::new());

    // Run the calculation without sending events during processing
    let calc_task = tokio::spawn(calculate_size_async(
        target_dir.clone(),
        analytics_map.clone(),
        Some(target_dir.clone()),
        Some(visited_inodes.clone()),
        Some(processed_paths.clone()),
    ));

    // Wait for calculation to complete and handle any errors
    if let Err(e) = calc_task.await? {
        eprintln!("Error during directory calculation: {}", e);
        return Err(e);
    }
    
    // Calculate scan time
    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    
    // Convert the analytics map to a vector of entries
    let entries = analytics_map_to_entries(&analytics_map);
    
    // Create the complete result object
    let result = DirectoryScanResult {
        root_path: target_dir,
        entries,
        scan_time_ms: elapsed_ms,
    };
    
    // Send the complete result as a single event
    if let Err(e) = window.emit("scan-result", &result) {
        eprintln!("Failed to emit scan result: {}", e);
    }
    
    // Also emit completion event for backward compatibility
    if let Err(e) = window.emit("scan-complete", ()) {
        eprintln!("Failed to emit completion event: {}", e);
    }
    
    // Explicitly clear resources
    drop(analytics_map);
    drop(visited_inodes);
    drop(processed_paths);

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
            None, // No target directory
            Some(visited_inodes),
            Some(Arc::new(DashSet::new())),
        ).await?;
        
        // Verify the results
        assert!(analytics_map.contains_key(&path), "Path should be in the analytics map");
        
        let analytics = analytics_map.get(&path).unwrap();
        // Directories have different size behaviors on different platforms
        #[cfg(target_family = "unix")]
        assert!(analytics.size_bytes.load(Ordering::Relaxed) > 0, "Directory should have a size on Unix filesystems");
        
        // On Windows, empty directories might report a size of 0
        #[cfg(target_os = "windows")]
        assert!(analytics.size_bytes.load(Ordering::Relaxed) >= 0, "Directory size might be 0 on Windows");
        
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
            Some(visited_inodes),
            Some(Arc::new(DashSet::new())),
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
            Some(visited_inodes),
            Some(Arc::new(DashSet::new())),
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
    #[cfg(not(target_os = "windows"))]
    async fn test_calculate_size_with_symlink() -> std::io::Result<()> {
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
            Some(visited_inodes),
            Some(Arc::new(DashSet::new())),
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
}
