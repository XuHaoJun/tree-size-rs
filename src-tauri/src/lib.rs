mod platform;
mod sector_reader;

use dashmap::{DashMap, DashSet};
use lazy_static::lazy_static;
use platform::PathInfo;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::Emitter;
use std::fs::File;
use ntfs::Ntfs;
use ntfs::indexes::NtfsFileNameIndex;
use std::time::SystemTime;
use std::io::{BufReader, Read, Seek, Write};

/// Contains analytics information for a directory or file
#[derive(Debug, Clone, Serialize)]
struct AnalyticsInfo {
  /// Path to the file or directory
  path: PathBuf,
  /// Total size in bytes
  size_bytes: u64,
  /// Total size in bytes on disk
  size_allocated_bytes: u64,
  /// Total number of entries (files and directories)
  entry_count: u64,
  /// Number of files
  file_count: u64,
  /// Number of directories
  directory_count: u64,
  /// Last modified time (Unix timestamp in seconds)
  last_modified_time: u64,
  /// Owner of the file or directory
  owner_name: Option<String>,
  /// Path info
  path_info: Option<PathInfo>,
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
  is_virtual_directory: bool,
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

// Function to detect if a path is on an NTFS volume
#[cfg(target_os = "windows")]
fn is_ntfs_volume(path: &Path) -> bool {
  use std::ffi::OsString;
  use std::os::windows::ffi::OsStringExt;
  use winapi::um::fileapi::{GetVolumeInformationW, GetVolumePathNameW};
  use winapi::shared::minwindef::MAX_PATH;
  
  // First get the volume path
  let path_str = path.to_string_lossy().to_string();
  let mut path_wide: Vec<u16> = path_str.encode_utf16().collect();
  path_wide.push(0); // null terminator
  
  let mut volume_path_wide = vec![0u16; MAX_PATH as usize];
  
  // Get volume path name
  let success = unsafe {
    GetVolumePathNameW(
      path_wide.as_ptr(),
      volume_path_wide.as_mut_ptr(),
      volume_path_wide.len() as u32,
    ) != 0
  };
  
  if !success {
    return false;
  }
  
  // Get filesystem type
  let mut fs_name_wide = vec![0u16; MAX_PATH as usize];
  let mut volume_serial = 0;
  let mut max_component_len = 0;
  let mut fs_flags = 0;
  
  let success = unsafe {
    GetVolumeInformationW(
      volume_path_wide.as_ptr(),
      std::ptr::null_mut(),
      0,
      &mut volume_serial,
      &mut max_component_len,
      &mut fs_flags,
      fs_name_wide.as_mut_ptr(),
      fs_name_wide.len() as u32,
    ) != 0
  };
  
  if !success {
    return false;
  }
  
  // Find the null terminator
  let len = fs_name_wide.iter().position(|&c| c == 0).unwrap_or(fs_name_wide.len());
  let fs_name_wide = &fs_name_wide[..len];
  
  // Convert from wide string to OsString then to string
  let fs_name = OsString::from_wide(fs_name_wide);
  let fs_name = fs_name.to_string_lossy().to_uppercase();
  
  fs_name == "NTFS"
}

#[cfg(not(target_os = "windows"))]
fn is_ntfs_volume(_path: &Path) -> bool {
  // NTFS detection not implemented for non-Windows platforms
  false
}

// Function to calculate size using the NTFS crate
fn calculate_size_ntfs(
  path: &Path,
  analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
  target_dir_path: &Path,
  visited_inodes: Arc<DashSet<(u64, u64)>>,
  processed_paths: Arc<DashSet<PathBuf>>,
) -> std::io::Result<()> {
  // Determine the volume path from the input path
  // let volume_path = {
  //   let path_str = path.to_string_lossy().to_string();
  //   if path_str.len() >= 2 && &path_str[1..3] == ":\\" {
  //     // Extract drive letter (e.g., "C:")
  //     // format!("{}:\\", &path_str[..1])
  //     format!("C:\\")
  //   } else {
  //     // Not a valid Windows volume path
  //     return Err(std::io::Error::new(
  //       std::io::ErrorKind::InvalidInput,
  //       "Not a valid Windows volume path",
  //     ));
  //   }
  // };
   let volume_path = "\\\\.\\C:";
  
  // Open the volume directly using ntfs crate
  let mut file = match File::open(volume_path) {
    Ok(f) => f,
    Err(e) => {
      eprintln!("Failed to open NTFS volume {}: {}", volume_path, e);
      return Err(e);
    }
  };

  let sr = sector_reader::SectorReader::new(file.try_clone()?, 4096)?;
  let mut fs = BufReader::new(sr);
  
  // Initialize NTFS filesystem
  let mut ntfs = match Ntfs::new(&mut fs) {
    Ok(ntfs) => ntfs,
    Err(e) => {
      eprintln!("Failed to initialize NTFS filesystem: {}", e);
      return Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("Failed to initialize NTFS: {}", e),
      ));
    }
  };
  ntfs.read_upcase_table(&mut fs)?;
  // We need to convert the target path to an NTFS path
  // First, remove the volume prefix from the path
  let rel_path = if let Some(remainder) = path.to_string_lossy().strip_prefix(&volume_path) {
    remainder.replace('\\', "/")
  } else {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidInput,
      "Path is not on the target volume",
    ));
  };
  
  // Process the path recursively
  process_ntfs_path(&ntfs, &mut file, &rel_path, path, analytics_map, target_dir_path, visited_inodes, processed_paths)
}

// Process a path within the NTFS filesystem
fn process_ntfs_path(
  ntfs: &Ntfs,
  file: &mut File,
  ntfs_path: &str,
  full_path: &Path,
  analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
  target_dir_path: &Path,
  visited_inodes: Arc<DashSet<(u64, u64)>>,
  processed_paths: Arc<DashSet<PathBuf>>,
) -> std::io::Result<()> {
  // If we've already processed this path, skip it
  if !processed_paths.insert(full_path.to_path_buf()) {
    return Ok(());
  }
  
  // First try to find the file by its path in the NTFS directory structure
  let root_dir = match ntfs.root_directory(file) {
    Ok(dir) => dir,
    Err(e) => {
      eprintln!("Failed to access NTFS root directory: {}", e);
      return Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("Failed to access NTFS root directory: {}", e),
      ));
    }
  };
  
  // Navigate to the specified path
  let ntfs_file = if ntfs_path.is_empty() || ntfs_path == "/" {
    // If the path is the root, use the root directory
    root_dir
  } else {
    // Use the NTFS directory index to find the file
    let index = match root_dir.directory_index(file) {
      Ok(idx) => idx,
      Err(e) => {
        eprintln!("Failed to get root directory index: {}", e);
        return Err(std::io::Error::new(
          std::io::ErrorKind::Other,
          format!("Failed to get root directory index: {}", e),
        ));
      }
    };
    
    // Create a finder for the index
    let mut _finder = index.finder();
    
    // Path components for traversal
    let components: Vec<&str> = ntfs_path.split('/').filter(|&c| !c.is_empty()).collect();
    if components.is_empty() {
      return Ok(());
    }
    
    // Start from the root and navigate the path
    let mut current_dir = root_dir;
    let mut current_file = None;
    
    for (i, component) in components.iter().enumerate() {
      let index = match current_dir.directory_index(file) {
        Ok(idx) => idx,
        Err(e) => {
          eprintln!("Failed to get directory index: {}", e);
          return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to get directory index: {}", e),
          ));
        }
      };
      
      let mut _finder = index.finder();
      
      match NtfsFileNameIndex::find(&mut _finder, ntfs, file, component) {
        Some(Ok(entry)) => {
          current_file = Some(entry.to_file(ntfs, file).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to convert entry to file: {}", e))
          })?);
          
          // If this isn't the last component and it's a directory, update current_dir
          if i < components.len() - 1 {
            if let Some(ref f) = current_file {
              if f.is_directory() {
                current_dir = f.clone();
              } else {
                return Err(std::io::Error::new(
                  std::io::ErrorKind::NotFound,
                  format!("Path component {} is not a directory", component),
                ));
              }
            }
          }
        }
        Some(Err(e)) => {
          return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Error finding NTFS file: {}", e),
          ));
        }
        None => {
          return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Path component not found: {}", component),
          ));
        }
      }
    }
    
    match current_file {
      Some(f) => f,
      None => {
        return Err(std::io::Error::new(
          std::io::ErrorKind::NotFound,
          "File not found in NTFS",
        ));
      }
    }
  };
  
  // Get NTFS file metadata
  let record_number = ntfs_file.file_record_number();
  let is_directory = ntfs_file.is_directory();
  let data_size = ntfs_file.data_size();
  let allocated_size = ntfs_file.allocated_size();
  
  // Use as inode identifier - NTFS record number and sequence number form a unique identifier
  let inode_pair = (record_number, ntfs_file.sequence_number() as u64);
  
  // Check for cycles using inode pair
  if !visited_inodes.insert(inode_pair) {
    // Already seen this inode, skip
    return Ok(());
  }
  
  // Counting entry as file or directory
  let entry_count = 1; // Count this file/directory as 1 entry
  let file_count = if is_directory { 0 } else { 1 };
  let directory_count = if is_directory { 1 } else { 0 };
  
  // Get last modified time from standard information attribute
  let last_modified_time;
  let mut owner_name = None;
  
  // Try to get standard information - this is safer than directly accessing attributes
  // Just use current time since we can't get the NTFS time information easily
  last_modified_time = SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or(std::time::Duration::from_secs(0))
    .as_secs();
  
  // Check for Windows owner (placeholder - would require additional logic to extract from NTFS security descriptor)
  // For now, use the same approach as in platform.rs
  let path_info = platform::get_path_info(&full_path, false);
  if let Some(info) = &path_info {
    owner_name = info.owner_name.clone();
  }
  
  // Add entry to analytics map with initial values
  let _entry_analytics = match analytics_map.entry(full_path.to_path_buf()) {
    dashmap::mapref::entry::Entry::Occupied(e) => e.get().clone(),
    dashmap::mapref::entry::Entry::Vacant(e) => {
      let analytics = Arc::new(AnalyticsInfo {
        path: full_path.to_path_buf(),
        size_bytes: data_size as u64, // Convert u32 to u64
        size_allocated_bytes: allocated_size as u64, // Convert u32 to u64
        entry_count,
        file_count,
        directory_count,
        last_modified_time,
        owner_name,
        path_info,
      });
      e.insert(analytics.clone());
      analytics
    }
  };
  
  // For directories, process all children
  if is_directory {
    // Get the directory index to iterate over children
    let dir_index = match ntfs_file.directory_index(file) {
      Ok(index) => index,
      Err(e) => {
        eprintln!("Failed to get directory index for {}: {}", full_path.display(), e);
        return Ok(());
      }
    };
    
    // Collect all the child paths and process them
    // First get all the children
    let mut child_paths = Vec::new();
    
    // Process the index entries one by one
    let mut index_iter = dir_index.entries(); 
    while let Some(Ok(entry)) = index_iter.next(file) {
      // Get the filename from the entry key
      if let Some(Ok(file_name)) = entry.key() {
        // Convert to String - handle possible error
        if let Ok(name_str) = file_name.name().to_string() {
          // Skip "." and ".." entries
          if name_str == "." || name_str == ".." {
            continue;
          }
          
          // Try to convert entry to file
          if let Ok(_) = entry.to_file(ntfs, file) {
            let child_path = full_path.join(&name_str);
            let child_ntfs_path = if ntfs_path.is_empty() || ntfs_path == "/" {
              format!("/{}", name_str)
            } else {
              format!("{}/{}", ntfs_path, name_str)
            };
            
            // Process the child recursively
            let _ = process_ntfs_path(
              ntfs,
              file,
              &child_ntfs_path,
              &child_path,
              analytics_map.clone(),
              target_dir_path,
              visited_inodes.clone(),
              processed_paths.clone(),
            );
            
            // Add to our list of processed paths
            child_paths.push(child_path);
          }
        }
      }
    }
    
    // Now recompute directory totals based on children
    let mut total_size = data_size as u64;
    let mut total_allocated_size = allocated_size as u64;
    let mut total_entries = 1;
    let mut total_files = 0;
    let mut total_dirs = 1;
    
    // Sum up all children's contributions
    for child_path in &child_paths {
      if let Some(child_analytics) = analytics_map.get(child_path) {
        total_size += child_analytics.size_bytes;
        total_allocated_size += child_analytics.size_allocated_bytes;
        total_entries += child_analytics.entry_count;
        total_files += child_analytics.file_count;
        total_dirs += child_analytics.directory_count;
      }
    }
    
    // Update the directory's analytics information
    if let Some(mut analytics_ref) = analytics_map.get_mut(&full_path.to_path_buf()) {
      let analytics = Arc::make_mut(&mut analytics_ref);
      analytics.size_bytes = total_size;
      analytics.size_allocated_bytes = total_allocated_size;
      analytics.entry_count = total_entries;
      analytics.file_count = total_files;
      analytics.directory_count = total_dirs;
    }
  }
  
  Ok(())
}

// Efficient sync function that uses Rayon for parallel processing
fn calculate_size_sync(
  path: &Path,
  analytics_map: Arc<DashMap<PathBuf, Arc<AnalyticsInfo>>>,
  target_dir_path: &Path,
  visited_inodes: Arc<DashSet<(u64, u64)>>,
  processed_paths: Arc<DashSet<PathBuf>>,
) -> std::io::Result<()> {
  // Check if the path is on an NTFS volume and use optimized NTFS calculation if possible
  #[cfg(target_os = "windows")]
  if is_ntfs_volume(path) {
    match calculate_size_ntfs(path, analytics_map.clone(), target_dir_path, visited_inodes.clone(), processed_paths.clone()) {
      Ok(_) => return Ok(()),
      Err(e) => {
        eprintln!("NTFS optimization failed, falling back to standard method: {}", e);
        // Fall through to standard calculation
      }
    }
  }

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
  let _entry_analytics = match analytics_map.entry(path.to_path_buf()) {
    dashmap::mapref::entry::Entry::Occupied(e) => e.get().clone(),
    dashmap::mapref::entry::Entry::Vacant(e) => {
      let analytics = Arc::new(AnalyticsInfo {
        path: path.to_path_buf(),
        size_bytes: path_info.size_bytes,
        size_allocated_bytes: path_info.size_allocated_bytes,
        entry_count,
        file_count,
        directory_count,
        last_modified_time: path_info.times.0 as u64,
        owner_name: path_info.owner_name.clone(),
        path_info: Some(path_info.clone()),
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
        target_dir_path,
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
        let child_size = child_analytics.size_bytes;
        let child_allocated_size = child_analytics.size_allocated_bytes;
        let child_entries = child_analytics.entry_count;
        let child_files = child_analytics.file_count;
        let child_dirs = child_analytics.directory_count;

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

    // Get a mutable reference to modify the Arc<AnalyticsInfo>
    if let Some(mut analytics_ref) = analytics_map.get_mut(&path.to_path_buf()) {
      // Access and modify the inner AnalyticsInfo fields
      let analytics = Arc::make_mut(&mut analytics_ref);

      // Update this directory's values
      analytics.size_bytes = total_size;
      analytics.size_allocated_bytes = total_allocated_size;
      analytics.entry_count = total_entries;
      analytics.file_count = total_files;
      analytics.directory_count = total_dirs;
    }
  }

  Ok(())
}

// This function converts the analytics map to a vector of AnalyticsInfo objects
fn analytics_map_to_entries(map: &DashMap<PathBuf, Arc<AnalyticsInfo>>) -> Vec<Arc<AnalyticsInfo>> {
  map
    .iter()
    .map(|item| item.value().clone()) // Just clone the Arc pointer, not the inner data
    .collect()
}

// This function builds a tree from the flat list of entries with a limited depth
fn build_tree_from_entries_with_depth(
  entries: &[Arc<AnalyticsInfo>],
  root_path: &Path,
  max_depth: usize,
  // Whether to build a virtual directory node for the root path
  // that contains all the children(files, no any dirs) of the root path
  // it is just a analytics node, not a real node in the file system
  // for example:
  // root_path: /Users/username/Documents
  // /Users/username/Documents/file1.txt
  // /Users/username/Documents/file2.txt
  // /Users/username/Documents/file3.txt
  // /Users/username/Documents/subdir/
  // will be a virtual directory node with 3 children file1, file2, file3
  // and will move file1, file2, file3 to the children of the virtual directory node
  // should total size of the virtual directory node is the sum of the size of file1, file2, file3
  build_virtual_directory_node: bool,
) -> FileSystemTreeNode {
  // Create a map of path -> entry for quick lookups
  let path_map: HashMap<PathBuf, &Arc<AnalyticsInfo>> = entries
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
    entry: &Arc<AnalyticsInfo>,
    children_map: &HashMap<PathBuf, Vec<PathBuf>>,
    path_map: &HashMap<PathBuf, &Arc<AnalyticsInfo>>,
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
      if let Some(child_paths) = children_map.get(path) {
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
      is_virtual_directory: false,
    }
  }

  // If we're not building a virtual directory node, build the tree normally
  // not build a virtual directory node if no files in the root path
  if !build_virtual_directory_node || (build_virtual_directory_node && root_entry.file_count == 0) {
    // Build the tree starting from the root with depth limit
    return build_node(
      &root_entry.path,
      root_entry,
      &children_map,
      &path_map,
      0,
      max_depth,
    );
  }

  // Building a virtual directory node
  // First, get all direct children of the root path
  let mut virtual_dir_children = Vec::new();
  let mut virtual_dir_size_bytes = 0;
  let mut virtual_dir_size_allocated_bytes = 0;
  let mut virtual_dir_entry_count = 0;
  let mut virtual_dir_file_count = 0;

  if let Some(child_paths) = children_map.get(&root_entry.path) {
    for child_path in child_paths {
      if let Some(child_entry) = path_map.get(child_path) {
        // Only include files (not directories) in the virtual directory
        if child_entry.file_count > 0 && child_entry.directory_count == 0 {
          // Create a tree node for this file
          let child_node = build_node(
            child_path,
            child_entry,
            &children_map,
            &path_map,
            0,
            0, // No children for files
          );

          // Update virtual directory stats
          virtual_dir_size_bytes += child_entry.size_bytes;
          virtual_dir_size_allocated_bytes += child_entry.size_allocated_bytes;
          virtual_dir_entry_count += 1;
          virtual_dir_file_count += 1;

          virtual_dir_children.push(child_node);
        }
      }
    }
  }

  // Sort virtual directory children by size (largest first)
  virtual_dir_children.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

  // Calculate percentages for virtual directory children
  for child in &mut virtual_dir_children {
    if virtual_dir_size_bytes > 0 {
      child.percent_of_parent = (child.size_bytes as f64 / virtual_dir_size_bytes as f64) * 100.0;
    } else {
      child.percent_of_parent = 0.0;
    }
  }

  // Create the virtual directory node
  // Extract the root directory name and append "Files" to it
  let root_name = root_entry
    .path
    .file_name()
    .and_then(|n| n.to_str())
    .unwrap_or("unknown");
  let virtual_dir_name = format!("{} Files", root_name);

  // Create a path for the virtual directory by appending the virtual directory name to parent path
  let virtual_dir_path = if let Some(parent) = root_entry.path.parent() {
    parent.join(&virtual_dir_name)
  } else {
    PathBuf::from(&virtual_dir_name)
  };

  let virtual_display_name = format!("[{} Files]", virtual_dir_file_count);

  let virtual_dir_node = FileSystemTreeNode {
    path: virtual_dir_path,
    name: virtual_display_name,
    size_bytes: virtual_dir_size_bytes,
    size_allocated_bytes: virtual_dir_size_allocated_bytes,
    entry_count: virtual_dir_entry_count,
    file_count: virtual_dir_file_count,
    directory_count: 0, // Virtual directory is not a real directory
    percent_of_parent: if root_entry.size_bytes > 0 {
      (virtual_dir_size_bytes as f64 / root_entry.size_bytes as f64) * 100.0
    } else {
      0.0
    },
    last_modified_time: root_entry.last_modified_time,
    owner_name: root_entry.owner_name.clone(),
    children: virtual_dir_children,
    is_virtual_directory: true,
  };

  // Now build the main tree but exclude the files that are in the virtual directory
  let mut main_tree = build_node(
    &root_entry.path,
    root_entry,
    &children_map,
    &path_map,
    0,
    max_depth,
  );

  // Filter the main tree's children to remove files (they're now in the virtual directory)
  main_tree.children.retain(|child| child.directory_count > 0);

  // Add the virtual directory as a child of the main tree
  main_tree.children.push(virtual_dir_node);

  // Resort the main tree's children by size
  main_tree
    .children
    .sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

  // Update percentages for all children
  for child in &mut main_tree.children {
    if main_tree.size_bytes > 0 {
      child.percent_of_parent = (child.size_bytes as f64 / main_tree.size_bytes as f64) * 100.0;
    } else {
      child.percent_of_parent = 0.0;
    }
  }

  main_tree
}

// This function builds a tree from prebuilt indices
fn build_tree_from_indices(
  entries: &[Arc<AnalyticsInfo>],
  path_map: &HashMap<PathBuf, usize>,
  children_map: &HashMap<PathBuf, Vec<usize>>,
  target_path: &Path,
  max_depth: usize,
  build_virtual_directory_node: bool,
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
    entry: &Arc<AnalyticsInfo>,
    entries: &[Arc<AnalyticsInfo>],
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
            is_virtual_directory: false,
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
      is_virtual_directory: false,
    }
  }

  // If not building a virtual directory node, just build the tree normally
  // not build a virtual directory node if no files in the target path
  if !build_virtual_directory_node || (build_virtual_directory_node && target_entry.file_count == 0)
  {
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

    return Some(node);
  }

  // We're building a virtual directory node
  // Get children indices for the target path
  let children_indices = match children_map.get(target_path) {
    Some(indices) => indices,
    None => return None,
  };

  // Collect file and directory children separately
  let mut file_indices = Vec::new();
  let mut dir_indices = Vec::new();

  for &idx in children_indices {
    let child_entry = &entries[idx];
    if child_entry.directory_count > 0 {
      dir_indices.push(idx);
    } else if child_entry.file_count > 0 {
      file_indices.push(idx);
    }
  }

  // First build the main tree node without the file children
  let mut main_tree = build_node(
    target_path,
    target_entry,
    entries,
    Some(&dir_indices),
    0,
    max_depth,
  );

  // If we have files, create a virtual directory node for them
  if !file_indices.is_empty() {
    // Calculate virtual directory statistics
    let mut virtual_dir_size_bytes = 0;
    let mut virtual_dir_size_allocated_bytes = 0;
    let virtual_dir_entry_count = file_indices.len() as u64;
    let virtual_dir_file_count = file_indices.len() as u64;

    // Create virtual directory children
    let mut virtual_dir_children = Vec::with_capacity(file_indices.len());

    for &idx in &file_indices {
      let file_entry = &entries[idx];

      // Create a node for each file
      let file_node = FileSystemTreeNode {
        path: file_entry.path.clone(),
        name: file_entry
          .path
          .file_name()
          .and_then(|n| n.to_str())
          .unwrap_or("unknown")
          .to_string(),
        size_bytes: file_entry.size_bytes,
        size_allocated_bytes: file_entry.size_allocated_bytes,
        entry_count: file_entry.entry_count,
        file_count: file_entry.file_count,
        directory_count: 0,
        percent_of_parent: 0.0, // Will be updated later
        last_modified_time: file_entry.last_modified_time,
        owner_name: file_entry.owner_name.clone(),
        children: Vec::new(),
        is_virtual_directory: false,
      };

      // Update virtual directory stats
      virtual_dir_size_bytes += file_entry.size_bytes;
      virtual_dir_size_allocated_bytes += file_entry.size_allocated_bytes;

      virtual_dir_children.push(file_node);
    }

    // Sort virtual directory children by size (largest first)
    virtual_dir_children.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    // Update percentages for virtual directory children
    for child in &mut virtual_dir_children {
      if virtual_dir_size_bytes > 0 {
        child.percent_of_parent = (child.size_bytes as f64 / virtual_dir_size_bytes as f64) * 100.0;
      }
    }

    // Extract the root directory name and append "Files" to it
    let root_name = target_path
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("unknown");
    let virtual_dir_name = format!("{} Files", root_name);

    // Create a path for the virtual directory
    let virtual_dir_path = if let Some(parent) = target_path.parent() {
      parent.join(&virtual_dir_name)
    } else {
      PathBuf::from(&virtual_dir_name)
    };

    let virtual_display_name = format!("[{} Files]", virtual_dir_file_count);

    // Create the virtual directory node
    let virtual_dir_node = FileSystemTreeNode {
      path: virtual_dir_path,
      name: virtual_display_name,
      size_bytes: virtual_dir_size_bytes,
      size_allocated_bytes: virtual_dir_size_allocated_bytes,
      entry_count: virtual_dir_entry_count,
      file_count: virtual_dir_file_count,
      directory_count: 0,
      percent_of_parent: if main_tree.size_bytes > 0 {
        (virtual_dir_size_bytes as f64 / main_tree.size_bytes as f64) * 100.0
      } else {
        0.0
      },
      last_modified_time: target_entry.last_modified_time,
      owner_name: target_entry.owner_name.clone(),
      children: virtual_dir_children,
      is_virtual_directory: true,
    };

    // Add the virtual directory as a child of the main tree
    main_tree.children.push(virtual_dir_node);

    // Re-sort the main tree's children by size
    main_tree
      .children
      .sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    // Update percentages for all children
    for child in &mut main_tree.children {
      if main_tree.size_bytes > 0 {
        child.percent_of_parent = (child.size_bytes as f64 / main_tree.size_bytes as f64) * 100.0;
      }
    }
  }

  Some(main_tree)
}

// Define a global cache to store scan results
lazy_static! {
  static ref GLOBAL_SCAN_CACHE: Mutex<Option<ScanCache>> = Mutex::new(None);
}

// Structure to hold cached scan data
struct ScanCache {
  root_path: PathBuf,
  entries: Vec<Arc<AnalyticsInfo>>,
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
      target_dir_clone.as_path(),
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
  let tree = build_tree_from_entries_with_depth(&entries, &target_dir, 1, true);

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
      build_indices(&entries_for_indices, &target_dir_for_indices)
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

// Function to build indices for faster tree building
fn build_indices(
  entries: &[Arc<AnalyticsInfo>],
  target_dir: &Path,
) -> (HashMap<PathBuf, usize>, HashMap<PathBuf, Vec<usize>>) {
  // First pass: build path_map (map from path to index in entries) - parallelize this
  let path_map = entries
    .par_iter()
    .enumerate()
    .map(|(i, entry)| (entry.path.clone(), i))
    .collect::<HashMap<_, _>>();

  // Second pass: build children_map
  // This is harder to fully parallelize so we'll use a concurrent map
  let children_map = DashMap::new();

  // Populate the children map in parallel
  entries.par_iter().enumerate().for_each(|(i, entry)| {
    if let Some(parent_path) = entry.path.parent().map(|p| p.to_path_buf()) {
      // Skip entries that are outside our target directory
      if !parent_path.starts_with(target_dir) && parent_path != *target_dir {
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
        let size_a = entries[a].size_bytes;
        let size_b = entries[b].size_bytes;
        size_b.cmp(&size_a) // Sort largest first
      });

      regular_children_map.insert(parent_path, indices);
    });

  (path_map, regular_children_map)
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
      1,    // Just show direct children
      true, // Build virtual directory node
    ) {
      return Ok(tree);
    }

    // If the optimized method failed (unlikely), fall back to the original method
    let entry = cache.entries.iter().find(|e| e.path == target_dir);

    if let Some(_) = entry {
      // Build a tree using the original method
      let tree = build_tree_from_entries_with_depth(&cache.entries, &target_dir, 1, true);
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
      path.as_path(),
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
      analytics.size_bytes > 0,
      "Directory should have a size on Unix filesystems"
    );

    // On Windows, empty directories might report a size of 0
    #[cfg(target_os = "windows")]
    assert!(
      analytics.size_bytes >= 0,
      "Directory size might be 0 on Windows"
    );

    assert_eq!(analytics.entry_count, 1, "Should count itself as one entry");
    assert_eq!(analytics.file_count, 0, "Should have 0 files");
    assert_eq!(
      analytics.directory_count, 1,
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
      path.as_path(),
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
      analytics.size_bytes >= test_data.len() as u64,
      "Directory size should include the file size"
    );
    assert_eq!(analytics.entry_count, 2, "Should count itself and one file");
    assert_eq!(analytics.file_count, 1, "Should have 1 file");
    assert_eq!(
      analytics.directory_count, 1,
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
      path.as_path(),
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
      root_analytics.size_bytes >= expected_size,
      "Directory size should include all files and subdirectories"
    );
    assert_eq!(
      root_analytics.entry_count, 4,
      "Should count root dir, subdir, and 2 files"
    );
    assert_eq!(root_analytics.file_count, 2, "Should have 2 files");
    assert_eq!(
      root_analytics.directory_count, 2,
      "Should count root and subdirectory"
    );

    // Verify the results for the subdirectory
    assert!(
      analytics_map.contains_key(&subdir_path),
      "Subdir path should be in the analytics map"
    );

    let subdir_analytics = analytics_map.get(&subdir_path).unwrap();
    assert!(
      subdir_analytics.size_bytes >= test_data.len() as u64,
      "Subdirectory size should include its file size"
    );
    assert_eq!(
      subdir_analytics.entry_count, 2,
      "Should count subdir and its file"
    );
    assert_eq!(subdir_analytics.file_count, 1, "Should have 1 file");
    assert_eq!(
      subdir_analytics.directory_count, 1,
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
      path.as_path(),
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
      analytics.entry_count >= 2,
      "Should count at least the directory and file"
    );
    assert_eq!(
      analytics.file_count, 1,
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
        test_dir.as_path(),
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
          test_dir.as_path(),
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
      path.as_path(),
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
      path.as_path(),
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