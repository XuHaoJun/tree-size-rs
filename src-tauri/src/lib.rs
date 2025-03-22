mod platform;

use dashmap::{DashMap, DashSet};
use lazy_static::lazy_static;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::Emitter;

/// Contains analytics information for a directory or file
#[derive(Debug)]
struct AnalyticsInfo {
  /// Total size in bytes
  size_bytes: AtomicU64,
  /// Total size in bytes on disk
  size_allocated_bytes: AtomicU64,
  /// Total number of entries (files and directories)
  entry_count: AtomicU64,
  /// Number of files
  file_count: AtomicU64,
  /// Number of directories
  directory_count: AtomicU64,
  /// Last modified time (Unix timestamp in seconds)
  last_modified_time: AtomicU64,
  /// Owner of the file or directory
  owner_name: Option<String>,
}

/// Represents size information for a file or directory
#[derive(Clone, Debug, Serialize)]
struct FileSystemEntry {
  /// Path to the file or directory
  path: PathBuf,
  /// Size in bytes
  size_bytes: u64,
  /// Size allocated on disk in bytes
  size_allocated_bytes: u64,
  /// Number of entries (files and directories)
  entry_count: u64,
  /// Number of files
  file_count: u64,
  /// Number of directories
  directory_count: u64,
  /// last modified time
  last_modified_time: u64,
  /// Owner of the file or directory
  owner_name: Option<String>,
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
  /// Size allocated on disk in bytes
  size_allocated_bytes: u64,
  /// Number of entries (files and directories)
  entry_count: u64,
  /// Number of files
  file_count: u64,
  /// Number of directories
  directory_count: u64,
  /// Percentage of parent size (0-100)
  percent_of_parent: f64,
  /// Last modified time (Unix timestamp in seconds)
  last_modified_time: u64,
  /// Owner of the file or directory
  owner_name: Option<String>,
  /// Child nodes
  children: Vec<FileSystemTreeNode>,
}

/// Complete scan result with tree representation
#[derive(Clone, Debug, Serialize)]
struct DirectoryScanResult {
  /// The root directory path
  root_path: PathBuf,
  /// Tree representation of the directory structure
  tree: FileSystemTreeNode,
  /// Total scan time in milliseconds
  scan_time_ms: u64,
}

// Efficient sync function that uses Rayon for parallel processing
fn calculate_size_sync(
  path: &Path,
  analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
  target_dir_path: Option<&Path>,
  visited_inodes: Arc<DashSet<(u64, u64)>>,
  processed_paths: Arc<DashSet<PathBuf>>,
) -> std::io::Result<()> {
  // If we've already processed this path, skip it
  if !processed_paths.insert(path.to_path_buf()) {
    return Ok(());
  }

  // Get path info using our platform-agnostic function - will work for files, dirs and symlinks
  let is_symlink = path.is_symlink();
  let path_info = match platform::get_path_info(path, is_symlink) {
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
  let directory_count = if path_info.is_dir && !is_symlink {
    1
  } else {
    0
  };

  // Add entry to analytics map with initial values (will be updated later for directories)
  let entry_analytics = match analytics_map.entry(path.to_path_buf()) {
    dashmap::mapref::entry::Entry::Occupied(e) => e.get().clone(),
    dashmap::mapref::entry::Entry::Vacant(e) => {
      let analytics = Arc::new(AnalyticsInfo {
        size_bytes: AtomicU64::new(path_info.size_bytes),
        size_allocated_bytes: AtomicU64::new(path_info.size_allocated_bytes),
        entry_count: AtomicU64::new(entry_count),
        file_count: AtomicU64::new(file_count),
        directory_count: AtomicU64::new(directory_count),
        last_modified_time: AtomicU64::new(path_info.times.0 as u64),
        owner_name: path_info.owner_name,
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
      }
      Err(_) => Vec::new(),
    };

    // Process all children in parallel using Rayon
    entries.par_iter().for_each(|child_path| {
      let _ = calculate_size_sync(
        child_path,
        analytics_map.clone(),
        target_dir_path.clone(),
        visited_inodes.clone(),
        processed_paths.clone(),
      );
    });

    // Now compute the total size based on children
    let dir_own_size = path_info.size_bytes; // Start with directory's own size
    let dir_own_allocated_size = path_info.size_allocated_bytes; // Start with directory's own allocated size
    let mut total_size = dir_own_size;
    let mut total_allocated_size = dir_own_allocated_size;
    let mut total_entries = 1; // Start with the directory itself
    let mut total_files = 0; // Directories don't count as files
    let mut total_dirs = 1; // Count this directory

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
        let child_allocated_size = child_analytics.size_allocated_bytes.load(Ordering::Relaxed);
        let child_entries = child_analytics.entry_count.load(Ordering::Relaxed);
        let child_files = child_analytics.file_count.load(Ordering::Relaxed);
        let child_dirs = child_analytics.directory_count.load(Ordering::Relaxed);

        total_size += child_size;
        total_allocated_size += child_allocated_size;

        // For symlinks, count the entry but not as file/dir
        if child_path.is_symlink() {
          total_entries += 1; // Count the symlink as an entry
        } else {
          // For non-symlinks, add all the counts
          if !child_is_file {
            // Avoid double counting files we already counted
            total_entries += child_entries;
            total_files += child_files;
          }
          total_dirs += child_dirs;
        }
      }
    }

    // Update this directory's values atomically (all at once)
    entry_analytics
      .size_bytes
      .store(total_size, Ordering::Relaxed);
    entry_analytics
      .size_allocated_bytes
      .store(total_allocated_size, Ordering::Relaxed);
    entry_analytics
      .entry_count
      .store(total_entries, Ordering::Relaxed);
    entry_analytics
      .file_count
      .store(total_files, Ordering::Relaxed);
    entry_analytics
      .directory_count
      .store(total_dirs, Ordering::Relaxed);
  }

  Ok(())
}

// This function converts the analytics map to a vector of FileSystemEntry objects
fn analytics_map_to_entries(map: &DashMap<PathBuf, Arc<AnalyticsInfo>>) -> Vec<FileSystemEntry> {
  map
    .iter()
    .map(|item| {
      let path = item.key();
      let analytics = item.value();

      FileSystemEntry {
        path: path.clone(),
        size_bytes: analytics.size_bytes.load(Ordering::Relaxed),
        size_allocated_bytes: analytics.size_allocated_bytes.load(Ordering::Relaxed),
        entry_count: analytics.entry_count.load(Ordering::Relaxed),
        file_count: analytics.file_count.load(Ordering::Relaxed),
        directory_count: analytics.directory_count.load(Ordering::Relaxed),
        last_modified_time: analytics.last_modified_time.load(Ordering::Relaxed) as u64,
        owner_name: analytics.owner_name.clone(),
      }
    })
    .collect()
}

// This function builds a tree from the flat list of entries with a limited depth
fn build_tree_from_entries_with_depth(
  entries: &[FileSystemEntry],
  root_path: &Path,
  max_depth: usize,
) -> FileSystemTreeNode {
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
      children_map
        .entry(parent_path)
        .or_default()
        .push(entry.path.clone());
    }
  }

  // Use the first entry if root not found
  let root_entry = root_entry.unwrap_or_else(|| {
    entries.first().unwrap_or_else(|| {
      panic!("No entries found to build tree");
    })
  });

  // Recursive function to build the tree with depth limit
  fn build_node(
    path: &Path,
    entry: &FileSystemEntry,
    children_map: &HashMap<PathBuf, Vec<PathBuf>>,
    path_map: &HashMap<PathBuf, &FileSystemEntry>,
    current_depth: usize,
    max_depth: usize,
  ) -> FileSystemTreeNode {
    // Extract name from path
    let name = path
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("unknown")
      .to_string();

    // Get children for this path if we haven't reached max depth
    let mut children = Vec::new();
    if current_depth < max_depth {
      if let Some(child_paths) = children_map.get(&entry.path) {
        for child_path in child_paths {
          if let Some(child_entry) = path_map.get(child_path) {
            let child_node = build_node(
              child_path,
              child_entry,
              children_map,
              path_map,
              current_depth + 1,
              max_depth,
            );
            children.push(child_node);
          }
        }

        // Sort children by size (largest first)
        children.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
      }
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
      size_allocated_bytes: entry.size_allocated_bytes,
      entry_count: entry.entry_count,
      file_count: entry.file_count,
      directory_count: entry.directory_count,
      percent_of_parent: 100.0, // Default value, will be updated by parent
      last_modified_time: entry.last_modified_time,
      owner_name: entry.owner_name.clone(),
      children,
    }
  }

  // Build the tree starting from the root with depth limit
  build_node(
    &root_entry.path,
    root_entry,
    &children_map,
    &path_map,
    0,
    max_depth,
  )
}

// This function builds a tree from prebuilt indices
fn build_tree_from_indices(
  entries: &[FileSystemEntry],
  path_map: &HashMap<PathBuf, usize>,
  children_map: &HashMap<PathBuf, Vec<usize>>,
  target_path: &Path,
  max_depth: usize,
) -> Option<FileSystemTreeNode> {
  // Find the index of the target path
  let target_index = match path_map.get(target_path) {
    Some(&idx) => idx,
    None => return None,
  };

  // Get the entry for the target path
  let target_entry = &entries[target_index];

  // Recursive function to build the tree starting from the target
  fn build_node(
    path: &Path,
    entry: &FileSystemEntry,
    entries: &[FileSystemEntry],
    children_indices: Option<&Vec<usize>>,
    current_depth: usize,
    max_depth: usize,
  ) -> FileSystemTreeNode {
    // Extract name from path
    let name = path
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("unknown")
      .to_string();

    // Get children for this path if we haven't reached max depth
    let mut children = Vec::new();
    if current_depth < max_depth {
      if let Some(indices) = children_indices {
        for &child_idx in indices {
          let child_entry = &entries[child_idx];

          let child_node = FileSystemTreeNode {
            path: child_entry.path.clone(),
            name: child_entry
              .path
              .file_name()
              .and_then(|n| n.to_str())
              .unwrap_or("unknown")
              .to_string(),
            size_bytes: child_entry.size_bytes,
            size_allocated_bytes: child_entry.size_allocated_bytes,
            entry_count: child_entry.entry_count,
            file_count: child_entry.file_count,
            directory_count: child_entry.directory_count,
            percent_of_parent: if entry.size_bytes > 0 {
              (child_entry.size_bytes as f64 / entry.size_bytes as f64) * 100.0
            } else {
              0.0
            },
            last_modified_time: child_entry.last_modified_time,
            owner_name: child_entry.owner_name.clone(),
            children: Vec::new(), // No need to build children of children here
          };

          children.push(child_node);
        }

        // No need to sort here - the indices are already sorted by size (largest first)
      }
    }

    FileSystemTreeNode {
      path: entry.path.clone(),
      name,
      size_bytes: entry.size_bytes,
      size_allocated_bytes: entry.size_allocated_bytes,
      entry_count: entry.entry_count,
      file_count: entry.file_count,
      directory_count: entry.directory_count,
      percent_of_parent: 100.0, // Default value, will be updated by parent
      last_modified_time: entry.last_modified_time,
      owner_name: entry.owner_name.clone(),
      children,
    }
  }

  // Build the tree node
  let children_indices = children_map.get(target_path);
  let node = build_node(
    target_path,
    target_entry,
    entries,
    children_indices,
    0,
    max_depth,
  );

  Some(node)
}

// Define a global cache to store scan results
lazy_static! {
  static ref GLOBAL_SCAN_CACHE: Mutex<Option<ScanCache>> = Mutex::new(None);
}

// Structure to hold cached scan data
struct ScanCache {
  root_path: PathBuf,
  entries: Vec<FileSystemEntry>,
  // Prebuilt indices for faster tree building
  path_map: HashMap<PathBuf, usize>, // Maps path to index in entries
  children_map: HashMap<PathBuf, Vec<usize>>, // Maps parent path to indices of children in entries
}

// New command to scan directory and return complete results at once
#[tauri::command]
async fn scan_directory_size(path: String, window: tauri::Window) -> Result<(), String> {
  // Drop any previous resources before starting a new scan
  tokio::task::yield_now().await;

  // Clear the global cache first when starting a new scan
  if let Ok(mut global_cache) = GLOBAL_SCAN_CACHE.lock() {
    *global_cache = None;
  }

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

// Modified scan_directory_complete function to store results in global cache
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
    // Run the synchronous calculation using Rayon's parallel processing
    calculate_size_sync(
      target_dir_clone.as_path(),
      analytics_map_clone.clone(),
      Some(target_dir_clone.as_path()),
      visited_inodes,
      processed_paths,
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

  // Build the initial tree from the entries with just a basic approach
  // This will be quick and allows us to show results to the user without waiting for indexing
  let tree = build_tree_from_entries_with_depth(&entries, &target_dir, 1);

  // Create the complete result object
  let result = DirectoryScanResult {
    root_path: target_dir.clone(),
    tree: tree.clone(),
    scan_time_ms: elapsed_ms,
  };

  // Send the complete result as a single event immediately
  if let Err(e) = window.emit("scan-result", &result) {
    eprintln!("Failed to emit scan result: {}", e);
  }

  // Also emit completion event for backward compatibility
  if let Err(e) = window.emit("scan-complete", ()) {
    eprintln!("Failed to emit completion event: {}", e);
  }

  // Now that the user sees the results, build the indices in the background
  // Clone what we need for the async task
  let entries_clone = entries.clone();
  let target_dir_clone = target_dir.clone();

  // Spawn a new async task to build the indices
  tokio::spawn(async move {
    // Clone values before passing to spawn_blocking to avoid ownership issues
    let target_dir_for_indices = target_dir_clone.clone();
    let entries_for_indices = entries_clone.clone();

    // Use tokio's spawn_blocking to run CPU-intensive parallelized work
    // This ensures we don't block the async runtime with CPU-bound work
    let indices_result = tokio::task::spawn_blocking(move || {
      // Prebuild indices for faster tree building

      // First pass: build path_map (map from path to index in entries) - parallelize this
      let path_map = entries_for_indices
        .par_iter()
        .enumerate()
        .map(|(i, entry)| (entry.path.clone(), i))
        .collect::<HashMap<_, _>>();

      // Second pass: build children_map
      // This is harder to fully parallelize so we'll use a concurrent map
      let children_map = DashMap::new();

      // Populate the children map in parallel
      entries_for_indices
        .par_iter()
        .enumerate()
        .for_each(|(i, entry)| {
          if let Some(parent_path) = entry.path.parent().map(|p| p.to_path_buf()) {
            // Skip entries that are outside our target directory
            if !parent_path.starts_with(&target_dir_for_indices)
              && parent_path != target_dir_for_indices
            {
              return;
            }

            // Use entry API on DashMap to safely add children indices
            children_map
              .entry(parent_path)
              .or_insert_with(Vec::new)
              .push(i);
          }
        });

      // Convert DashMap to regular HashMap for storage
      let mut regular_children_map: HashMap<PathBuf, Vec<usize>> =
        HashMap::with_capacity(children_map.len());

      // Third pass: collect and sort children by size (largest first) for each parent
      children_map
        .into_iter()
        .for_each(|(parent_path, mut indices)| {
          // Sort indices by size (largest first) - this can be done in parallel for each entry
          indices.par_sort_unstable_by(|&a, &b| {
            let size_a = entries_for_indices[a].size_bytes;
            let size_b = entries_for_indices[b].size_bytes;
            size_b.cmp(&size_a) // Sort largest first
          });

          regular_children_map.insert(parent_path, indices);
        });

      (path_map, regular_children_map)
    })
    .await;

    // Process the result of the parallel work
    if let Ok((path_map, children_map)) = indices_result {
      // Store the results in the global cache
      let cache = ScanCache {
        root_path: target_dir_clone,
        entries: entries_clone,
        path_map,
        children_map,
      };

      // Update the global cache
      if let Ok(mut global_cache) = GLOBAL_SCAN_CACHE.lock() {
        *global_cache = Some(cache);
      } else {
        eprintln!("Failed to acquire lock on global cache");
      }
    } else {
      eprintln!("Failed to build indices in background task");
    }
  });

  Ok(())
}

// Updated get_directory_children function to use cached data
#[tauri::command]
async fn get_directory_children(path: String) -> Result<FileSystemTreeNode, String> {
  // Access the global cache
  let cache_guard = GLOBAL_SCAN_CACHE
    .lock()
    .map_err(|e| format!("Failed to acquire cache lock: {}", e))?;

  // Check if we have cached scan data
  if let Some(cache) = &*cache_guard {
    // Convert the path to canonical form
    let target_dir = match Path::new(&path).canonicalize() {
      Ok(p) => p,
      Err(e) => return Err(format!("Failed to canonicalize path: {}", e)),
    };

    // Check if the requested path is within our cached data (it should be a subpath of the root)
    if !target_dir.starts_with(&cache.root_path) && target_dir != cache.root_path {
      return Err(format!(
        "Path {} is not within the scanned directory {}",
        target_dir.display(),
        cache.root_path.display()
      ));
    }

    // Use the prebuilt indices to build the tree (much faster)
    if let Some(tree) = build_tree_from_indices(
      &cache.entries,
      &cache.path_map,
      &cache.children_map,
      &target_dir,
      1, // Just show direct children
    ) {
      return Ok(tree);
    }

    // If the optimized method failed (unlikely), fall back to the original method
    let entry = cache.entries.iter().find(|e| e.path == target_dir);

    if let Some(_) = entry {
      // Build a tree using the original method
      let tree = build_tree_from_entries_with_depth(&cache.entries, &target_dir, 1);
      return Ok(tree);
    } else {
      return Err(format!(
        "Path {} not found in scan data",
        target_dir.display()
      ));
    }
  } else {
    // No cached data available, need to perform a fresh scan
    return Err("No scan data available. Please scan a directory first.".to_string());
  }
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

// Command to clear the scan cache
#[tauri::command]
async fn clear_scan_cache() -> Result<(), String> {
  if let Ok(mut global_cache) = GLOBAL_SCAN_CACHE.lock() {
    *global_cache = None;
    Ok(())
  } else {
    Err("Failed to acquire lock on global cache".to_string())
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
      get_space_info,
      get_directory_children, // Add the new command
      clear_scan_cache        // Add cache clearing command
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

    // Run the function
    calculate_size_sync(
      path.as_path(),
      analytics_map.clone(),
      None, // No target directory
      visited_inodes,
      processed_paths,
    )?;

    // Verify the results
    assert!(
      analytics_map.contains_key(&path),
      "Path should be in the analytics map"
    );

    let analytics = analytics_map.get(&path).unwrap();
    // Directories have different size behaviors on different platforms
    #[cfg(target_family = "unix")]
    assert!(
      analytics.size_bytes.load(Ordering::Relaxed) > 0,
      "Directory should have a size on Unix filesystems"
    );

    // On Windows, empty directories might report a size of 0
    #[cfg(target_os = "windows")]
    assert!(
      analytics.size_bytes.load(Ordering::Relaxed) >= 0,
      "Directory size might be 0 on Windows"
    );

    assert_eq!(
      analytics.entry_count.load(Ordering::Relaxed),
      1,
      "Should count itself as one entry"
    );
    assert_eq!(
      analytics.file_count.load(Ordering::Relaxed),
      0,
      "Should have 0 files"
    );
    assert_eq!(
      analytics.directory_count.load(Ordering::Relaxed),
      1,
      "Should count itself as one directory"
    );

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

    // Run the function
    calculate_size_sync(
      path.as_path(),
      analytics_map.clone(),
      None,
      visited_inodes,
      processed_paths,
    )?;

    // Verify the results
    assert!(
      analytics_map.contains_key(&path),
      "Path should be in the analytics map"
    );

    let analytics = analytics_map.get(&path).unwrap();
    assert!(
      analytics.size_bytes.load(Ordering::Relaxed) >= test_data.len() as u64,
      "Directory size should include the file size"
    );
    assert_eq!(
      analytics.entry_count.load(Ordering::Relaxed),
      2,
      "Should count itself and one file"
    );
    assert_eq!(
      analytics.file_count.load(Ordering::Relaxed),
      1,
      "Should have 1 file"
    );
    assert_eq!(
      analytics.directory_count.load(Ordering::Relaxed),
      1,
      "Should count itself as one directory"
    );

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

    // Run the function
    calculate_size_sync(
      path.as_path(),
      analytics_map.clone(),
      None,
      visited_inodes,
      processed_paths,
    )?;

    // Verify the results for the root directory
    assert!(
      analytics_map.contains_key(&path),
      "Root path should be in the analytics map"
    );

    let root_analytics = analytics_map.get(&path).unwrap();
    let expected_size = (test_data.len() + root_test_data.len()) as u64;
    assert!(
      root_analytics.size_bytes.load(Ordering::Relaxed) >= expected_size,
      "Directory size should include all files and subdirectories"
    );
    assert_eq!(
      root_analytics.entry_count.load(Ordering::Relaxed),
      4,
      "Should count root dir, subdir, and 2 files"
    );
    assert_eq!(
      root_analytics.file_count.load(Ordering::Relaxed),
      2,
      "Should have 2 files"
    );
    assert_eq!(
      root_analytics.directory_count.load(Ordering::Relaxed),
      2,
      "Should count root and subdirectory"
    );

    // Verify the results for the subdirectory
    assert!(
      analytics_map.contains_key(&subdir_path),
      "Subdir path should be in the analytics map"
    );

    let subdir_analytics = analytics_map.get(&subdir_path).unwrap();
    assert!(
      subdir_analytics.size_bytes.load(Ordering::Relaxed) >= test_data.len() as u64,
      "Subdirectory size should include its file size"
    );
    assert_eq!(
      subdir_analytics.entry_count.load(Ordering::Relaxed),
      2,
      "Should count subdir and its file"
    );
    assert_eq!(
      subdir_analytics.file_count.load(Ordering::Relaxed),
      1,
      "Should have 1 file"
    );
    assert_eq!(
      subdir_analytics.directory_count.load(Ordering::Relaxed),
      1,
      "Should count itself as one directory"
    );

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

    // Run the function
    calculate_size_sync(
      path.as_path(),
      analytics_map.clone(),
      None,
      visited_inodes,
      processed_paths,
    )?;

    // Verify the results
    assert!(
      analytics_map.contains_key(&path),
      "Path should be in the analytics map"
    );

    let analytics = analytics_map.get(&path).unwrap();
    // The symlink is counted in the entry count, but not as a file
    // In some implementations, symlinks might be treated differently
    assert!(
      analytics.entry_count.load(Ordering::Relaxed) >= 2,
      "Should count at least the directory and file"
    );
    assert_eq!(
      analytics.file_count.load(Ordering::Relaxed),
      1,
      "Should count only the actual file, not the symlink as a file"
    );

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

      // Use Rayon's default thread pool (parallel)
      calculate_size_sync(
        test_dir.as_path(),
        analytics_map.clone(),
        None,
        visited_inodes,
        processed_paths,
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

      // Create a single-threaded pool to simulate sequential processing
      let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();

      pool.install(|| {
        let _ = calculate_size_sync(
          test_dir.as_path(),
          analytics_map.clone(),
          None,
          visited_inodes,
          processed_paths,
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

  #[tokio::test]
  #[cfg(target_family = "unix")]
  async fn test_owner_name_unix() -> std::io::Result<()> {
    // Create a temporary directory with a file
    let temp_dir = tempdir()?;
    let path = temp_dir.path().to_path_buf();
    let file_path = path.join("test_owner.txt");
    let mut file = File::create(&file_path)?;
    file.write_all(b"Testing owner")?;

    // Get path info directly
    let path_info = platform::get_path_info(&file_path, false);
    assert!(path_info.is_some(), "Should get path info");

    let info = path_info.unwrap();
    assert!(info.owner_name.is_some(), "Should have owner name on Unix");

    // Current user should be the owner of the file we just created
    let current_user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME"));
    if let Ok(username) = current_user {
      assert_eq!(
        info.owner_name.as_deref(),
        Some(username.as_str()),
        "File should be owned by current user"
      );
    }

    // Test through the analytics map
    let analytics_map = Arc::new(DashMap::new());
    let visited_inodes = Arc::new(DashSet::new());
    let processed_paths = Arc::new(DashSet::new());

    calculate_size_sync(
      path.as_path(),
      analytics_map.clone(),
      None,
      visited_inodes,
      processed_paths,
    )?;

    // Convert to entries and check owner_name is preserved
    let entries = analytics_map_to_entries(&analytics_map);
    let file_entry = entries.iter().find(|e| e.path == file_path);

    assert!(file_entry.is_some(), "File entry should exist");
    if let Some(entry) = file_entry {
      assert!(
        entry.owner_name.is_some(),
        "Owner name should be present in entry"
      );
    }

    Ok(())
  }

  #[tokio::test]
  #[cfg(target_os = "windows")]
  async fn test_owner_name_windows() -> std::io::Result<()> {
    use std::fs::metadata;

    // Create a temporary directory with a file
    let temp_dir = tempdir()?;
    let path = temp_dir.path().to_path_buf();
    let file_path = path.join("test_owner.txt");
    let mut file = File::create(&file_path)?;
    file.write_all(b"Testing owner")?;

    // Get metadata directly first to verify file creation
    let file_metadata = metadata(&file_path)?;
    assert!(file_metadata.is_file(), "Should be a file");

    // Get path info directly with debug output
    println!("Attempting to get path info for: {:?}", file_path);
    let path_info = platform::get_path_info(&file_path, false);

    match &path_info {
      Some(info) => {
        println!("Got path info with owner: {:?}", info.owner_name);
        assert!(
          info.owner_name.is_some(),
          "Should have owner name on Windows"
        );
      }
      None => {
        panic!("Failed to get path info for file");
      }
    }

    // Test through the analytics map
    let analytics_map = Arc::new(DashMap::new());
    let visited_inodes = Arc::new(DashSet::new());
    let processed_paths = Arc::new(DashSet::new());

    calculate_size_sync(
      path.as_path(),
      analytics_map.clone(),
      None,
      visited_inodes,
      processed_paths,
    )?;

    // Convert to entries and check owner_name is preserved
    let entries = analytics_map_to_entries(&analytics_map);
    let file_entry = entries.iter().find(|e| e.path == file_path);

    match file_entry {
      Some(entry) => {
        println!("Found file entry with owner: {:?}", entry.owner_name);
        assert!(
          entry.owner_name.is_some(),
          "Owner name should be present in entry"
        );
        assert!(
          !entry.owner_name.as_ref().unwrap().is_empty(),
          "Owner name should not be empty"
        );
      }
      None => {
        panic!("File entry not found in analytics map");
      }
    }

    Ok(())
  }
}
