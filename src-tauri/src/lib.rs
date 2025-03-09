use dashmap::DashMap;
use kanal;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::fs;
use dashmap::DashSet;

/// Contains analytics information for a directory or file
#[derive(Debug)]
struct AnalyticsInfo {
    /// Total size in bytes
    size_bytes: AtomicU64,
    /// Total number of entries (files and directories)
    entry_count: AtomicU64,
}

impl AnalyticsInfo {
    fn new() -> Self {
        Self {
            size_bytes: AtomicU64::new(0),
            entry_count: AtomicU64::new(0),
        }
    }

    fn new_with_size(size: u64) -> Self {
        Self {
            size_bytes: AtomicU64::new(size),
            entry_count: AtomicU64::new(1), // Count itself as 1 entry
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
}

// Asynchronous recursive processing function
async fn calculate_size_async(
    path: PathBuf,
    analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
    sender: Option<kanal::Sender<FileSystemEntry>>,
    target_dir_path: Option<PathBuf>,
    visited_inodes: Option<Arc<DashSet<(u64, u64)>>>, // Track device and inode pairs
) -> std::io::Result<()> {
    // Handle symlinks
    if path.is_symlink() {
        // Only count the symlink itself, not what it points to
        if let Ok(metadata) = fs::symlink_metadata(&path).await {
            let self_size = metadata.len();
            update_analytics(&path, self_size, 1, &analytics_map, &sender);
        }
        return Ok(());
    }

    // Asynchronously get metadata
    let metadata = match fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };

    // Check for cycles using device and inode numbers on Unix-like systems
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let inode = metadata.ino();
        let dev = metadata.dev();
        let inode_pair = (dev, inode);

        let visited = visited_inodes.clone().unwrap_or_else(|| Arc::new(DashSet::new()));
        
        // If we've seen this inode before, we have a cycle
        if !visited.insert(inode_pair) && metadata.is_dir() {
            return Ok(());
        }
    }

    let self_size = metadata.len();
    let is_dir = metadata.is_dir();
    let entry_count = 1; // Count this file/directory as 1

    // Handle updates directly
    let mut updates = vec![];
    if is_dir {
        match analytics_map.entry(path.clone()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                updates.push((e.get().clone(), self_size, entry_count, path.clone()));
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let analytics = Arc::new(AnalyticsInfo::new_with_size(self_size));
                updates.push((analytics.clone(), 0, 0, path.clone()));
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
                analytics.size_bytes.fetch_add(self_size, Ordering::Relaxed);
                analytics.entry_count.fetch_add(entry_count, Ordering::Relaxed);
                
                // Send parent path and values to channel if available
                if let Some(sender) = &sender {
                    let _ = sender.send(FileSystemEntry {
                        path: parent.clone(),
                        size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
                        entry_count: analytics.entry_count.load(Ordering::Relaxed),
                    });
                }
            }
        }
    }

    // Process updates
    for (analytics, size, count, path) in updates {
        analytics.size_bytes.fetch_add(size, Ordering::Relaxed);
        analytics.entry_count.fetch_add(count, Ordering::Relaxed);
        
        // Send path and values to channel if available
        if let Some(sender) = &sender {
            let _ = sender.send(FileSystemEntry {
                path: path,
                size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
                entry_count: analytics.entry_count.load(Ordering::Relaxed),
            });
        }
    }

    // Process subdirectories asynchronously
    if is_dir {
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
    count: u64,
    analytics_map: &Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
    sender: &Option<kanal::Sender<FileSystemEntry>>
) {
    if let Some(analytics) = analytics_map.get(path) {
        analytics.size_bytes.fetch_add(size, Ordering::Relaxed);
        analytics.entry_count.fetch_add(count, Ordering::Relaxed);
        
        // Send path and values to channel if available
        if let Some(sender) = sender {
            let _ = sender.send(FileSystemEntry {
                path: path.to_path_buf(),
                size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
                entry_count: analytics.entry_count.load(Ordering::Relaxed),
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

// 异步获取父目录链
async fn get_parent_chain(path: &Path) -> Vec<PathBuf> {
    get_parent_chain_sync(path)
}

// New command to stream directory sizes to the frontend
#[tauri::command]
async fn scan_directory_size(path: String, window: tauri::Window) -> Result<(), String> {
    match scan_directory_with_events(path, window).await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

async fn scan_directory_with_events(path: String, window: tauri::Window) -> std::io::Result<()> {
    let target_dir = Path::new(&path).canonicalize()?;
    let analytics_map = Arc::new(DashMap::new());
    // Initialize the set to track visited inodes (to prevent cycles)
    let visited_inodes = Arc::new(DashSet::new());

    // Warm up parent directory chain
    // let path_chain = get_parent_chain(&target_dir).await;
    // for p in path_chain {
    //     size_map.entry(p).or_insert(Arc::new(AtomicU64::new(0)));
    // }

    // Create a channel with capacity 100 for sending file system entries
    let (sender, receiver) = kanal::bounded::<FileSystemEntry>(100);

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

    // Run the calculate_size_async function in a separate task
    let calc_task = tokio::spawn(calculate_size_async(
        target_dir.clone(),
        analytics_map.clone(),
        Some(sender),
        Some(target_dir),
        Some(visited_inodes),
    ));

    // Wait for calculation to complete and handle any errors
    if let Err(e) = calc_task.await? {
        eprintln!("Error during directory calculation: {}", e);
        return Err(e);
    }

    // Wait for receiver task to complete
    let _ = receiver_handle.await;

    Ok(())
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
        .invoke_handler(tauri::generate_handler![scan_directory_size])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
