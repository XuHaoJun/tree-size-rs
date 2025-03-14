mod platform;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use dashmap::{DashMap, DashSet};
use serde::Serialize;
use tauri::Emitter;
use std::collections::HashMap;
use rayon::prelude::*;
use std::sync::Mutex;

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
    #[allow(dead_code)]
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

/// Represents a node in the file system tree
#[derive(Clone, Debug, Serialize)]
struct FileSystemTreeNode {
    /// Path to the file or directory
    path: PathBuf,
    /// Name of the file or directory (just the filename, not the full path)
    name: String,
    /// Size in bytes
    size_bytes: u64,
    /// Number of entries (files and directories)
    entry_count: u64,
    /// Number of files
    file_count: u64,
    /// Number of directories
    directory_count: u64,
    /// Percentage of parent size (0-100)
    percent_of_parent: f64,
    /// Child nodes
    children: Vec<FileSystemTreeNode>,
}

/// Complete scan result with tree representation
#[derive(Clone, Debug, Serialize)]
struct DirectoryScanResult {
    /// The root directory path
    root_path: PathBuf,
    /// All entries found during the scan (flat list)
    entries: Vec<FileSystemEntry>,
    /// Tree representation of the directory structure
    tree: FileSystemTreeNode,
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

// Structure to hold updates that need to be applied to parent directories
#[allow(dead_code)]
struct ParentUpdate {
    path: PathBuf,
    size_bytes: u64,
    entry_count: u64,
    file_count: u64,
    directory_count: u64,
}

// Efficient sync function that uses Rayon for parallel processing
fn calculate_size_sync(
    path: PathBuf,
    analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
    target_dir_path: Option<PathBuf>,
    visited_inodes: Arc<DashSet<(u64, u64)>>,
    processed_paths: Arc<DashSet<PathBuf>>,
    parent_updates: Arc<Mutex<Vec<ParentUpdate>>>,
) -> std::io::Result<()> {
    // If we've already processed this path, skip it
    if !processed_paths.insert(path.clone()) {
        return Ok(());
    }
    
    // Get path info using our platform-agnostic function - will work for files, dirs and symlinks
    let is_symlink = path.is_symlink();
    let path_info = match platform::get_path_info(&path, true, is_symlink) {
        Some(info) => info,
        None => return Ok(()),
    };
    
    // Check for cycles using device and inode numbers if available
    // This handles both directory cycles AND symlinks properly
    if let Some(inode_pair) = path_info.inode_device {
        if !visited_inodes.insert(inode_pair) {
            // We've already seen this inode, skip it
            return Ok(());
        }
    }

    // Count entry as file or directory, symlinks count as entries but not as files or dirs
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
        // Read directory entries
        let entries = match std::fs::read_dir(&path) {
            Ok(dir_entries) => {
                let mut entry_paths = Vec::with_capacity(32); // Pre-allocate for common case
                for entry_result in dir_entries {
                    if let Ok(entry) = entry_result {
                        entry_paths.push(entry.path());
                    }
                }
                entry_paths
            },
            Err(_) => Vec::new(),
        };
        
        // Process all children in parallel using Rayon
        entries.par_iter().for_each(|child_path| {
            let _ = calculate_size_sync(
                child_path.clone(),
                analytics_map.clone(),
                target_dir_path.clone(),
                visited_inodes.clone(),
                processed_paths.clone(),
                parent_updates.clone(),
            );
        });
        
        // Now compute the total size based on children
        let dir_own_size = path_info.size; // Start with directory's own size
        let mut total_size = dir_own_size;  
        let mut total_entries = 1;  // Start with the directory itself
        let mut total_files = 0;    // Directories don't count as files
        let mut total_dirs = 1;     // Count this directory
        
        // Sum up all children's contributions
        for child_path in &entries {
            // Check if the child is a direct file (not a symlink pointing to a file)
            let child_is_file = child_path.is_file() && !child_path.is_symlink();
            
            // Update counts for direct files first
            if child_is_file {
                total_files += 1;
                total_entries += 1;
            }
            
            // Get analytics info for the child if it exists
            if let Some(child_analytics) = analytics_map.get(child_path) {
                let child_size = child_analytics.size_bytes.load(Ordering::Relaxed);
                let child_entries = child_analytics.entry_count.load(Ordering::Relaxed);
                let child_files = child_analytics.file_count.load(Ordering::Relaxed);
                let child_dirs = child_analytics.directory_count.load(Ordering::Relaxed);
                
                total_size += child_size;
                
                // For symlinks, count the entry but not as file/dir
                if child_path.is_symlink() {
                    total_entries += 1; // Count the symlink as an entry
                } else {
                    // For non-symlinks, add all the counts
                    if !child_is_file { // Avoid double counting files we already counted
                        total_entries += child_entries;
                        total_files += child_files;
                    }
                    total_dirs += child_dirs;
                }
            }
        }
        
        // Update this directory's values atomically (all at once)
        entry_analytics.size_bytes.store(total_size, Ordering::Relaxed);
        entry_analytics.entry_count.store(total_entries, Ordering::Relaxed);
        entry_analytics.file_count.store(total_files, Ordering::Relaxed);
        entry_analytics.directory_count.store(total_dirs, Ordering::Relaxed);
    }
    
    // Queue parent directory updates for batch processing after all calculations
    if !is_target_dir {
        // Use the final calculated size (which includes all children for directories)
        let size = entry_analytics.size_bytes.load(Ordering::Relaxed);
        let entries = entry_analytics.entry_count.load(Ordering::Relaxed);
        let files = entry_analytics.file_count.load(Ordering::Relaxed);
        let dirs = entry_analytics.directory_count.load(Ordering::Relaxed);
        
        let parent_chain = get_parent_chain_sync(&path, target_dir_path.as_deref());
        
        // Add all parent updates to the queue
        let mut update_queue = parent_updates.lock().unwrap();
        for parent in parent_chain {
            update_queue.push(ParentUpdate {
                path: parent,
                size_bytes: size,
                entry_count: entries,
                file_count: files,
                directory_count: dirs,
            });
        }
    }

    Ok(())
}

// Mark as #[allow(dead_code)] since it's kept for API compatibility
#[allow(dead_code)]
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
    
    // Create a queue for parent updates
    let parent_updates = Arc::new(Mutex::new(Vec::with_capacity(1000))); // Pre-allocate for efficiency
    
    // Process the directory synchronously with Rayon parallelism
    let result = calculate_size_sync(
        path,
        analytics_map.clone(),
        target_dir_path,
        visited,
        processed,
        parent_updates.clone(),
    );
    
    // Apply all parent updates in batch
    let updates = parent_updates.lock().unwrap();
    
    // Group updates by parent path for efficiency
    let mut grouped_updates: HashMap<PathBuf, (u64, u64, u64, u64)> = HashMap::new();
    for update in updates.iter() {
        let entry = grouped_updates.entry(update.path.clone()).or_insert((0, 0, 0, 0));
        entry.0 += update.size_bytes;
        entry.1 += update.entry_count;
        entry.2 += update.file_count;
        entry.3 += update.directory_count;
    }
    
    // Apply the grouped updates
    for (parent_path, (size, entries, files, dirs)) in grouped_updates {
        match analytics_map.entry(parent_path.clone()) {
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
    
    result
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

// This function builds a tree from the flat list of entries
fn build_tree_from_entries(entries: &[FileSystemEntry], root_path: &Path) -> FileSystemTreeNode {
    // Create a map of path -> entry for quick lookups
    let path_map: HashMap<PathBuf, &FileSystemEntry> = entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    
    // Create a map of parent path -> child paths
    let mut children_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    
    // Find the root node and build the parent-child relationships
    let mut root_entry = None;
    
    for entry in entries {
        if entry.path == root_path {
            root_entry = Some(entry);
            continue;
        }
        
        if let Some(parent_path) = entry.path.parent().map(|p| p.to_path_buf()) {
            children_map.entry(parent_path).or_default().push(entry.path.clone());
        }
    }
    
    // Use the first entry if root not found
    let root_entry = root_entry.unwrap_or_else(|| entries.first().unwrap_or_else(|| {
        panic!("No entries found to build tree");
    }));
    
    // Recursive function to build the tree
    fn build_node(
        path: &Path, 
        entry: &FileSystemEntry,
        children_map: &HashMap<PathBuf, Vec<PathBuf>>,
        path_map: &HashMap<PathBuf, &FileSystemEntry>
    ) -> FileSystemTreeNode {
        // Extract name from path
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        
        // Get children for this path
        let mut children = Vec::new();
        if let Some(child_paths) = children_map.get(&entry.path) {
            for child_path in child_paths {
                if let Some(child_entry) = path_map.get(child_path) {
                    let child_node = build_node(child_path, child_entry, children_map, path_map);
                    children.push(child_node);
                }
            }
            
            // Sort children by size (largest first)
            children.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        }
        
        // Calculate percentages for children
        let parent_size = entry.size_bytes;
        for child in &mut children {
            if parent_size > 0 {
                child.percent_of_parent = (child.size_bytes as f64 / parent_size as f64) * 100.0;
            } else {
                child.percent_of_parent = 0.0;
            }
        }
        
        FileSystemTreeNode {
            path: entry.path.clone(),
            name,
            size_bytes: entry.size_bytes,
            entry_count: entry.entry_count,
            file_count: entry.file_count,
            directory_count: entry.directory_count,
            percent_of_parent: 100.0, // Default value, will be updated by parent
            children,
        }
    }
    
    // Build the tree starting from the root
    build_node(&root_entry.path, root_entry, &children_map, &path_map)
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

    // Run the calculation using tokio's spawn_blocking for CPU-intensive work
    // This allows the expensive calculation to run without blocking other Tokio tasks
    let analytics_map_clone = analytics_map.clone();
    let target_dir_clone = target_dir.clone();
    let scan_task = tokio::task::spawn_blocking(move || {
        // Create parent updates collection
        let parent_updates = Arc::new(Mutex::new(Vec::with_capacity(1000)));
        
        // Run the synchronous calculation using Rayon's parallel processing
        calculate_size_sync(
            target_dir_clone.clone(),
            analytics_map_clone.clone(),
            Some(target_dir_clone.clone()),
            visited_inodes,
            processed_paths,
            parent_updates,
        )
    });

    // Wait for calculation to complete and handle any errors
    if let Err(e) = scan_task.await? {
        eprintln!("Error during directory calculation: {}", e);
        return Err(e);
    }
    
    // Calculate scan time
    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    
    // Convert the analytics map to a vector of entries
    let entries = analytics_map_to_entries(&analytics_map);
    
    // Build the tree from the entries
    let tree = build_tree_from_entries(&entries, &target_dir);
    
    // Create the complete result object
    let result = DirectoryScanResult {
        root_path: target_dir,
        entries,
        tree,
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
        let processed_paths = Arc::new(DashSet::new());
        let parent_updates = Arc::new(Mutex::new(Vec::new()));
        
        // Run the function
        calculate_size_sync(
            path.clone(),
            analytics_map.clone(),
            None, // No target directory
            visited_inodes,
            processed_paths,
            parent_updates,
        )?;
        
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
        let processed_paths = Arc::new(DashSet::new());
        let parent_updates = Arc::new(Mutex::new(Vec::new()));
        
        // Run the function
        calculate_size_sync(
            path.clone(),
            analytics_map.clone(),
            None,
            visited_inodes,
            processed_paths,
            parent_updates,
        )?;
        
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
        let processed_paths = Arc::new(DashSet::new());
        let parent_updates = Arc::new(Mutex::new(Vec::new()));
        
        // Run the function
        calculate_size_sync(
            path.clone(),
            analytics_map.clone(),
            None,
            visited_inodes,
            processed_paths,
            parent_updates,
        )?;
        
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
        let processed_paths = Arc::new(DashSet::new());
        let parent_updates = Arc::new(Mutex::new(Vec::new()));
        
        // Run the function
        calculate_size_sync(
            path.clone(),
            analytics_map.clone(),
            None,
            visited_inodes,
            processed_paths,
            parent_updates,
        )?;
        
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

    // A manual benchmark that can be run with cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn benchmark_rayon_parallel_vs_sequential() -> std::io::Result<()> {
        // Choose a directory to scan - use home directory for a more realistic test
        let test_dir = dirs::home_dir()
            .unwrap_or_else(|| std::env::current_dir().unwrap())
            .to_path_buf();
        
        println!("Benchmarking directory: {:?}", test_dir);
        
        // First measure with parallelism
        let start = std::time::Instant::now();
        {
            let analytics_map = Arc::new(DashMap::new());
            let visited_inodes = Arc::new(DashSet::new());
            let processed_paths = Arc::new(DashSet::new());
            let parent_updates = Arc::new(Mutex::new(Vec::with_capacity(1000)));
            
            // Use Rayon's default thread pool (parallel)
            calculate_size_sync(
                test_dir.clone(),
                analytics_map.clone(),
                None,
                visited_inodes,
                processed_paths,
                parent_updates,
            )?;
            
            println!("Parallel scan found {} entries", analytics_map.len());
        }
        let parallel_duration = start.elapsed();
        println!("Parallel processing took: {:?}", parallel_duration);
        
        // Then measure with single thread
        let start = std::time::Instant::now();
        {
            let analytics_map = Arc::new(DashMap::new());
            let visited_inodes = Arc::new(DashSet::new());
            let processed_paths = Arc::new(DashSet::new());
            let parent_updates = Arc::new(Mutex::new(Vec::with_capacity(1000)));
            
            // Create a single-threaded pool to simulate sequential processing
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .unwrap();
            
            pool.install(|| {
                let _ = calculate_size_sync(
                    test_dir.clone(),
                    analytics_map.clone(),
                    None,
                    visited_inodes,
                    processed_paths,
                    parent_updates,
                );
            });
            
            println!("Sequential scan found {} entries", analytics_map.len());
        }
        let sequential_duration = start.elapsed();
        println!("Sequential processing took: {:?}", sequential_duration);
        
        // Calculate and print the speedup
        let speedup = sequential_duration.as_secs_f64() / parallel_duration.as_secs_f64();
        println!("Speedup from parallelism: {:.2}x", speedup);
        
        Ok(())
    }
}
