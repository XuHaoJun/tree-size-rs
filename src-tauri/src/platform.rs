#[allow(unused_imports)]
use std::fs;

use std::path::Path;

use serde::Serialize;

#[cfg(target_family = "unix")]
fn get_block_size() -> u64 {
  // All os specific implementations of MetadataExt seem to define a block as 512 bytes
  // https://doc.rust-lang.org/std/os/linux/fs/trait.MetadataExt.html#tymethod.st_blocks
  512
}

type InodeAndDevice = (u64, u64);
type FileTime = (i64, i64, i64);

/// Represents complete information about a filesystem path
#[derive(Debug, Clone, Serialize)]
pub struct PathInfo {
  /// Size in bytes
  pub size_bytes: u64,
  // Size in bytes on disk
  pub size_allocated_bytes: u64,
  /// Inode and device information for detecting cycles (Unix) or file/volume ID (Windows)
  pub inode_device: Option<(u64, u64)>,
  /// File modification/access/creation times
  pub times: (i64, i64, i64),
  /// Whether the path is a directory
  pub is_dir: bool,
  // Whether the path is a file
  pub is_file: bool,
  // Whether the path is a symlink
  #[allow(dead_code)]
  pub is_symlink: bool,
  // The owner of the path
  pub owner_name: Option<String>,
}

/// Get complete path information in a platform-agnostic way
pub fn get_path_info<P: AsRef<Path>>(path: P, follow_links: bool) -> Option<PathInfo> {
  let path_ref = path.as_ref();

  // First get the metadata
  let (size_bytes, size_allocated_bytes, inode_device, times) =
    get_metadata(path_ref, follow_links)?;

  // Then determine if it's a directory
  let metadata = if follow_links {
    fs::metadata(path_ref).ok()?
  } else {
    fs::symlink_metadata(path_ref).ok()?
  };

  let is_dir = metadata.is_dir();
  let is_file = metadata.is_file();
  let is_symlink = metadata.file_type().is_symlink();

  // Get the owner name
  let owner_name = get_owner_name(path_ref, &metadata);

  Some(PathInfo {
    size_bytes,
    size_allocated_bytes,
    inode_device,
    times,
    is_dir,
    is_file,
    is_symlink,
    owner_name,
  })
}

#[cfg(target_family = "unix")]
pub fn get_metadata<P: AsRef<Path>>(
  path: P,
  follow_links: bool,
) -> Option<(u64, u64, Option<InodeAndDevice>, FileTime)> {
  use std::os::unix::fs::MetadataExt;
  let metadata = if follow_links {
    path.as_ref().metadata()
  } else {
    path.as_ref().symlink_metadata()
  };
  match metadata {
    Ok(md) => {
      // Apparent size
      let size = md.len();
      // Allocated size
      let size_allocated = md.blocks() * get_block_size();

      Some((
        size,
        size_allocated,
        Some((md.ino(), md.dev())),
        (md.mtime(), md.atime(), md.ctime()),
      ))
    }
    Err(_e) => None,
  }
}

#[cfg(target_family = "windows")]
fn windows_time_to_unix_time(windows_time: i64) -> i64 {
  // Windows time is in 100ns intervals since 1601-01-01
  // Convert to seconds and adjust for Unix epoch (1970-01-01)
  (windows_time / 10_000_000) - 11644473600
}

#[cfg(target_family = "windows")]
// Constants for MFT and NTFS
const SECTOR_SIZE: u64 = 512;
const BOOTSTRAP_SIZE: usize = 512;
const BYTES_PER_FILE_RECORD_SEGMENT: usize = 0x400;
const FILE_NAME_ATTR_TYPE: u32 = 0x30;
const DATA_ATTR_TYPE: u32 = 0x80;

#[cfg(target_family = "windows")]
#[repr(C, packed)]
struct NtfsBootRecord {
  jump: [u8; 3],
  oem_id: [u8; 8],
  bytes_per_sector: u16,
  sectors_per_cluster: u8,
  reserved_sectors: u16,
  always_zero_1: [u8; 3],
  unused_1: u16,
  media_descriptor: u8,
  always_zero_2: u16,
  sectors_per_track: u16,
  number_of_heads: u16,
  hidden_sectors: u32,
  unused_2: u32,
  unused_3: u32,
  total_sectors: u64,
  mft_logical_cluster_number: u64,
  mft_mirror_logical_cluster_number: u64,
  clusters_per_file_record_segment: i8,
  unused_4: [u8; 3],
  clusters_per_index_block: u8,
  unused_5: [u8; 3],
  volume_serial_number: u64,
  checksum: u32,
}

#[cfg(target_family = "windows")]
// Attempt to get file size from MFT for a specific file
fn get_file_size_from_mft(path: &Path) -> Option<(u64, u64)> {
  use std::fs::File;
  use std::io::{Read, Seek, SeekFrom};
  use std::ffi::OsString;
  use std::os::windows::ffi::OsStringExt;
  use winapi::um::fileapi::{GetVolumeInformationW, GetVolumePathNameW};
  use std::path::PathBuf;
  use std::collections::HashMap;
  
  // Enable debug logging to help trace and troubleshoot MFT reading
  const DEBUG_MFT: bool = false;
  
  if DEBUG_MFT {
    eprintln!("Attempting to get file size from MFT for: {:?}", path);
  }
  
  // First, get the volume path for the file
  let path_str = path.to_string_lossy().to_string();
  let wide_path: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();
  
  let mut volume_path_buffer = vec![0u16; 261]; // MAX_PATH + 1
  
  let result = unsafe {
    GetVolumePathNameW(
      wide_path.as_ptr(),
      volume_path_buffer.as_mut_ptr(),
      volume_path_buffer.len() as u32,
    )
  };
  
  if result == 0 {
    if DEBUG_MFT {
      eprintln!("Failed to get volume path for: {:?}", path);
    }
    return None;
  }
  
  // Truncate to the actual volume path null-terminated string
  let volume_path_len = volume_path_buffer.iter().position(|&c| c == 0).unwrap_or(volume_path_buffer.len());
  volume_path_buffer.truncate(volume_path_len);
  
  // Get volume information to check if it's NTFS
  let mut fs_name_buffer = vec![0u16; 50];
  let mut volume_serial_number = 0;
  let mut max_component_length = 0;
  let mut fs_flags = 0;
  
  let result = unsafe {
    GetVolumeInformationW(
      volume_path_buffer.as_ptr(),
      std::ptr::null_mut(),
      0,
      &mut volume_serial_number,
      &mut max_component_length,
      &mut fs_flags,
      fs_name_buffer.as_mut_ptr(),
      fs_name_buffer.len() as u32,
    )
  };
  
  if result == 0 {
    if DEBUG_MFT {
      eprintln!("Failed to get volume information");
    }
    return None;
  }
  
  // Convert fs_name to OsString to check if it's NTFS
  let fs_name_len = fs_name_buffer.iter().position(|&c| c == 0).unwrap_or(fs_name_buffer.len());
  fs_name_buffer.truncate(fs_name_len);
  let fs_name = OsString::from_wide(&fs_name_buffer);
  let fs_name_str = fs_name.to_string_lossy().to_uppercase();
  
  // Check if the filesystem is NTFS
  if fs_name_str != "NTFS" {
    if DEBUG_MFT {
      eprintln!("Filesystem is not NTFS, it's: {}", fs_name_str);
    }
    return None;
  }
  
  // Extract drive letter from volume path (usually first character)
  let volume_path_str = OsString::from_wide(&volume_path_buffer).to_string_lossy().to_string();
  let drive_letter = match volume_path_str.chars().next() {
    Some(c) if c.is_ascii_alphabetic() => c,
    _ => {
      if DEBUG_MFT {
        eprintln!("Could not extract drive letter from volume path: {}", volume_path_str);
      }
      return None;
    }
  };
  
  if DEBUG_MFT {
    eprintln!("Drive letter: {}, Volume path: {}", drive_letter, volume_path_str);
  }
  
  // Read the NTFS boot record
  let boot_record = match read_ntfs_boot_record(drive_letter) {
    Some(record) => record,
    None => {
      if DEBUG_MFT {
        eprintln!("Failed to read NTFS boot record for drive: {}", drive_letter);
      }
      return None;
    }
  };
  
  // Calculate MFT location
  let bytes_per_sector = boot_record.bytes_per_sector as u64;
  let sectors_per_cluster = boot_record.sectors_per_cluster as u64;
  let bytes_per_cluster = bytes_per_sector * sectors_per_cluster;
  let mft_offset = boot_record.mft_logical_cluster_number * bytes_per_cluster;
  
  // Determine MFT record size
  let file_record_size = if boot_record.clusters_per_file_record_segment < 0 {
    BYTES_PER_FILE_RECORD_SEGMENT / (1 << -boot_record.clusters_per_file_record_segment as i32) as usize
  } else {
    BYTES_PER_FILE_RECORD_SEGMENT * boot_record.clusters_per_file_record_segment as usize
  };
  
  if DEBUG_MFT {
    eprintln!("MFT offset: {}, Bytes per cluster: {}, File record size: {}", 
              mft_offset, bytes_per_cluster, file_record_size);
  }
  
  // Open the drive directly
  let drive_path = format!(r"\\.\{}:", drive_letter);
  let mut file = match File::open(drive_path) {
    Ok(file) => file,
    Err(e) => {
      if DEBUG_MFT {
        eprintln!("Failed to open drive {}: {}", drive_letter, e);
      }
      return None;
    }
  };
  
  // Get relative path from the volume root
  let relative_path = match path.strip_prefix(PathBuf::from(volume_path_str)) {
    Ok(rel_path) => rel_path,
    Err(_) => {
      if DEBUG_MFT {
        eprintln!("Could not get relative path from: {:?}", path);
      }
      return None;
    }
  };
  
  if DEBUG_MFT {
    eprintln!("Relative path: {:?}", relative_path);
  }
  
  // Collect path components for better matching
  let path_components: Vec<_> = relative_path.components().collect();
  if path_components.is_empty() {
    if DEBUG_MFT {
      eprintln!("No path components found");
    }
    return None;
  }
  
  // First, read the MFT record for the $MFT file itself (record 0)
  if let Err(e) = file.seek(SeekFrom::Start(mft_offset)) {
    if DEBUG_MFT {
      eprintln!("Failed to seek to MFT offset: {}", e);
    }
    return None;
  }
  
  let mut buffer = vec![0u8; file_record_size];
  if let Err(e) = file.read_exact(&mut buffer) {
    if DEBUG_MFT {
      eprintln!("Failed to read MFT record: {}", e);
    }
    return None;
  }
  
  // Check the signature "FILE"
  if &buffer[0..4] != b"FILE" {
    if DEBUG_MFT {
      eprintln!("Invalid MFT record signature: {:?}", &buffer[0..4]);
    }
    return None;
  }
  
  // First scan to build a map of record IDs to indices
  let max_records = 10000;
  let mut records_scanned = 0;
  let mut record_map = HashMap::new();
  let mut file_candidates = Vec::new();
  
  // First pass: build a record index
  for i in 5..max_records {  // Start from 5 (root directory)
    let offset = mft_offset + (i as u64 * file_record_size as u64);
    if let Err(e) = file.seek(SeekFrom::Start(offset)) {
      if DEBUG_MFT {
        eprintln!("Failed to seek to MFT record {}: {}", i, e);
      }
      break;
    }
    
    let mut record_buffer = vec![0u8; file_record_size];
    if let Err(_) = file.read_exact(&mut record_buffer) {
      if DEBUG_MFT {
        eprintln!("Failed to read MFT record {}", i);
      }
      break;
    }
    
    records_scanned += 1;
    
    // Skip non-valid records
    if &record_buffer[0..4] != b"FILE" {
      continue;
    }
    
    // Parse the record
    match parse_file_record(&record_buffer) {
      Ok(record) => {
        if record.is_deleted {
          continue;
        }
        
        // Store the record ID to index mapping
        let record_index = i as u64;
        record_map.insert(record_index, (record.file_name.clone(), record.parent_directory_reference & 0x0000FFFFFFFFFFFF));
        
        // If this is a file (not a directory) and it matches our target filename, add to candidates
        if !record.is_directory {
          if let Some(target_file_name) = path_components.last().and_then(|c| c.as_os_str().to_str()) {
            if record.file_name == target_file_name {
              file_candidates.push((record_index, record));
            }
          }
        }
      },
      Err(_) => continue,
    }
  }
  
  if DEBUG_MFT {
    eprintln!("Built record map with {} entries, found {} file candidates", 
              record_map.len(), file_candidates.len());
  }
  
  // If we have multiple candidates, check their parent directories
  if file_candidates.len() > 1 {
    for (record_index, record) in file_candidates {
      // Try to verify the full path by walking up parent references
      let mut current_parent_ref = record.parent_directory_reference & 0x0000FFFFFFFFFFFF;
      let mut path_match = true;
      let mut current_depth = path_components.len() - 1;
      
      // Walk up the parent chain
      while current_parent_ref != 5 && current_depth > 0 {  // 5 is root directory
        current_depth -= 1;
        
        if let Some((parent_name, grandparent_ref)) = record_map.get(&current_parent_ref) {
          // Check if this parent directory name matches our path component
          if let Some(expected_name) = path_components.get(current_depth).and_then(|c| c.as_os_str().to_str()) {
            if parent_name != expected_name {
              path_match = false;
              break;
            }
          }
          
          current_parent_ref = *grandparent_ref;
        } else {
          path_match = false;
          break;
        }
      }
      
      if path_match {
        if DEBUG_MFT {
          eprintln!("Full path match found! Returning file size for record {}", record_index);
        }
        return Some((record.file_size, record.allocated_size));
      }
    }
  } else if file_candidates.len() == 1 {
    // If only one candidate, just use that
    let (_, record) = &file_candidates[0];
    if DEBUG_MFT {
      eprintln!("Single match found! Name: {}, Size: {}, Allocated: {}", 
                record.file_name, record.file_size, record.allocated_size);
    }
    return Some((record.file_size, record.allocated_size));
  }
  
  if DEBUG_MFT {
    eprintln!("Scanned {} MFT records without finding a good path match for {:?}", 
              records_scanned, relative_path);
  }
  
  // If we didn't find a match, return None to fall back to regular approach
  None
}

#[cfg(target_family = "windows")]
// Attempt to read the NTFS boot record from a drive
fn read_ntfs_boot_record(drive_letter: char) -> Option<NtfsBootRecord> {
  use std::fs::File;
  use std::io::{Read, Seek, SeekFrom};
  use std::mem;

  // Format the drive path for raw access
  let drive_path = format!(r"\\.\{}:", drive_letter);
  
  // Attempt to open the drive
  let mut file = match File::open(drive_path) {
    Ok(file) => file,
    Err(_) => return None,
  };

  // Read the boot sector
  let mut buffer = [0u8; BOOTSTRAP_SIZE];
  if file.seek(SeekFrom::Start(0)).is_err() {
    return None;
  }
  
  if file.read_exact(&mut buffer).is_err() {
    return None;
  }
  
  // Check if this is an NTFS volume
  if buffer[3..11] != *b"NTFS    " {
    return None;
  }
  
  // Create a boot record from the raw bytes
  let boot_record: NtfsBootRecord = unsafe { mem::transmute_copy(&buffer) };
  
  Some(boot_record)
}

#[cfg(target_family = "windows")]
// Parse an MFT file record
fn parse_file_record(buffer: &[u8]) -> Result<FileRecord, Box<dyn std::error::Error>> {
  // Check signature "FILE"
  if &buffer[0..4] != b"FILE" {
    return Err("Invalid record signature".into());
  }
  
  // Extract basic record information
  let flags = u16::from_le_bytes([buffer[22], buffer[23]]);
  let is_directory = flags & 0x0002 != 0;
  let is_deleted = flags & 0x0001 == 0;
  
  let attributes_offset = u16::from_le_bytes([buffer[20], buffer[21]]) as usize;
  
  let mut file_name = String::new();
  let mut file_size = 0;
  let mut allocated_size = 0;
  let mut parent_directory_reference = 0;
  
  // Process attributes
  let mut offset = attributes_offset;
  while offset < buffer.len() - 8 {
    let attr_type = u32::from_le_bytes([buffer[offset], buffer[offset + 1], buffer[offset + 2], buffer[offset + 3]]);
    if attr_type == 0xFFFFFFFF {
      break; // End of attributes
    }
    
    let attr_length = u32::from_le_bytes([buffer[offset + 4], buffer[offset + 5], buffer[offset + 6], buffer[offset + 7]]) as usize;
    if attr_length == 0 || offset + attr_length > buffer.len() {
      break; // Invalid attribute length
    }
    
    let non_resident_flag = buffer[offset + 8];
    let resident = non_resident_flag == 0;
    
    // Process based on attribute type
    if attr_type == FILE_NAME_ATTR_TYPE && resident {
      // $FILE_NAME attribute
      let content_offset = u16::from_le_bytes([buffer[offset + 20], buffer[offset + 21]]) as usize;
      let attr_content_offset = offset + content_offset;
      
      if attr_content_offset + 8 < buffer.len() {
        parent_directory_reference = u64::from_le_bytes([
          buffer[attr_content_offset],
          buffer[attr_content_offset + 1],
          buffer[attr_content_offset + 2],
          buffer[attr_content_offset + 3],
          buffer[attr_content_offset + 4],
          buffer[attr_content_offset + 5],
          buffer[attr_content_offset + 6],
          buffer[attr_content_offset + 7],
        ]);
        
        if attr_content_offset + 64 < buffer.len() {
          let name_length = buffer[attr_content_offset + 64] as usize;
          let name_offset = attr_content_offset + 66;
          
          if name_offset + name_length * 2 <= buffer.len() {
            // Convert UTF-16 to String
            for i in 0..name_length {
              let char_code = u16::from_le_bytes([
                buffer[name_offset + i * 2],
                buffer[name_offset + i * 2 + 1],
              ]);
              if let Some(c) = std::char::from_u32(char_code as u32) {
                file_name.push(c);
              }
            }
          }
        }
      }
    } else if attr_type == DATA_ATTR_TYPE {
      // $DATA attribute
      if resident {
        let content_length = u32::from_le_bytes([
          buffer[offset + 16],
          buffer[offset + 17],
          buffer[offset + 18],
          buffer[offset + 19],
        ]) as u64;
        file_size = content_length;
        allocated_size = content_length;
      } else {
        // Non-resident attribute
        if offset + 56 < buffer.len() {
          // Real (apparent) file size
          file_size = u64::from_le_bytes([
            buffer[offset + 48],
            buffer[offset + 49],
            buffer[offset + 50],
            buffer[offset + 51],
            buffer[offset + 52],
            buffer[offset + 53],
            buffer[offset + 54],
            buffer[offset + 55],
          ]);
          
          // Allocated size (disk usage)
          allocated_size = u64::from_le_bytes([
            buffer[offset + 40],
            buffer[offset + 41],
            buffer[offset + 42],
            buffer[offset + 43],
            buffer[offset + 44],
            buffer[offset + 45],
            buffer[offset + 46],
            buffer[offset + 47],
          ]);
        }
      }
    }
    
    offset += attr_length;
  }
  
  Ok(FileRecord {
    is_directory,
    is_deleted,
    file_size,
    allocated_size,
    file_name,
    parent_directory_reference,
  })
}

#[cfg(target_family = "windows")]
#[derive(Debug)]
struct FileRecord {
  is_directory: bool,
  is_deleted: bool,
  file_size: u64,         // Apparent size (logical size)
  allocated_size: u64,    // Allocated size (physical size) 
  file_name: String,
  parent_directory_reference: u64,
}

#[cfg(target_family = "windows")]
pub fn get_metadata<P: AsRef<Path>>(
  path: P,
  follow_links: bool,
) -> Option<(u64, u64, Option<InodeAndDevice>, FileTime)> {
  use std::io;
  use winapi_util::Handle;
  
  let path_ref = path.as_ref();
  
  // First, try to get file size from MFT if it's an NTFS volume
  if let Some((apparent_size, allocated_size)) = get_file_size_from_mft(path_ref) {
    // Get basic metadata for other info like times
    let metadata = if follow_links {
      path_ref.metadata().ok()?
    } else {
      path_ref.symlink_metadata().ok()?
    };
    
    use std::os::windows::fs::MetadataExt;
    return Some((
      apparent_size,
      allocated_size,
      None, // We don't have file ID from MFT in this simplified implementation
      (
        windows_time_to_unix_time(metadata.last_write_time() as i64),
        windows_time_to_unix_time(metadata.last_access_time() as i64),
        windows_time_to_unix_time(metadata.creation_time() as i64),
      ),
    ));
  }
  
  // If MFT reading failed or it's not an NTFS volume, fall back to standard approach
  
  fn handle_from_path_limited(path: &Path) -> io::Result<Handle> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;

    let file = OpenOptions::new()
      .access_mode(FILE_READ_ATTRIBUTES)
      .open(path)?;
    Ok(Handle::from_file(file))
  }

  fn get_metadata_expensive(path: &Path) -> Option<(u64, u64, Option<InodeAndDevice>, FileTime)> {
    use filesize::PathExt;
    use winapi_util::file::information;

    let h = handle_from_path_limited(path).ok()?;
    let info = information(&h).ok()?;

    // Get both sizes
    let apparent_size = info.file_size();
    let allocated_size = path.size_on_disk().unwrap_or(apparent_size);

    Some((
      apparent_size,
      allocated_size,
      Some((info.file_index(), info.volume_serial_number())),
      (
        windows_time_to_unix_time(info.last_write_time().unwrap() as i64),
        windows_time_to_unix_time(info.last_access_time().unwrap() as i64),
        windows_time_to_unix_time(info.creation_time().unwrap() as i64),
      ),
    ))
  }

  use std::os::windows::fs::MetadataExt;
  let metadata = if follow_links {
    path_ref.metadata()
  } else {
    path_ref.symlink_metadata()
  };
  match metadata {
    Ok(ref md) => {
      const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
      const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
      const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
      const FILE_ATTRIBUTE_SYSTEM: u32 = 0x04;
      const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
      const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
      const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x00000200;
      const FILE_ATTRIBUTE_PINNED: u32 = 0x00080000;
      const FILE_ATTRIBUTE_UNPINNED: u32 = 0x00100000;
      const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x00040000;
      const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x00400000;
      const FILE_ATTRIBUTE_OFFLINE: u32 = 0x00001000;
      // normally FILE_ATTRIBUTE_SPARSE_FILE would be enough, however Windows sometimes likes to mask it out. see: https://stackoverflow.com/q/54560454
      const IS_PROBABLY_ONEDRIVE: u32 = FILE_ATTRIBUTE_SPARSE_FILE
        | FILE_ATTRIBUTE_PINNED
        | FILE_ATTRIBUTE_UNPINNED
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        | FILE_ATTRIBUTE_OFFLINE;
      let attr_filtered = md.file_attributes()
        & !(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM);
      if ((attr_filtered & FILE_ATTRIBUTE_ARCHIVE) != 0
        || (attr_filtered & FILE_ATTRIBUTE_DIRECTORY) != 0
        || md.file_attributes() == FILE_ATTRIBUTE_NORMAL)
        && !(attr_filtered & IS_PROBABLY_ONEDRIVE != 0)
      {
        // For normal files, we use the standard metadata
        let apparent_size = md.len();

        // For simple files, apparent size is often the same as allocated size
        // But we would need an expensive call to get the exact allocated size
        // We'll just use apparent size for both in this simple case
        let allocated_size = apparent_size;

        Some((
          apparent_size,
          allocated_size,
          None,
          (
            windows_time_to_unix_time(md.last_write_time() as i64),
            windows_time_to_unix_time(md.last_access_time() as i64),
            windows_time_to_unix_time(md.creation_time() as i64),
          ),
        ))
      } else {
        // For special files (compressed, sparse, etc.), we need the expensive call
        get_metadata_expensive(path_ref)
      }
    }
    _ => get_metadata_expensive(path_ref),
  }
}

/// Get disk space information for a given path
/// Returns a tuple of (total_space, available_space, used_space) in bytes
/// If the information can't be retrieved, returns None
pub fn get_space_info<P: AsRef<Path>>(path: P) -> Option<(u64, u64, u64)> {
  use sysinfo::Disks;

  let path_ref = path.as_ref();

  // First, ensure the path exists
  let metadata = get_path_info(path_ref, false)?;

  // Get the canonical path
  let canonical_path = path_ref.canonicalize().ok()?;

  // If it's a file, get the parent directory
  let dir_path = if metadata.is_file {
    canonical_path.parent()?.to_path_buf()
  } else {
    canonical_path
  };

  // Get all disk information
  let disks = Disks::new_with_refreshed_list();

  // Find the disk that contains our path
  for disk in &disks {
    let mount_point = disk.mount_point();

    // Check if our path is on this disk
    // In Unix-like systems, we check if our path starts with the mount point
    // In Windows, we compare drive letters
    #[cfg(target_family = "unix")]
    let on_this_disk = dir_path.starts_with(mount_point);

    #[cfg(target_os = "windows")]
    let on_this_disk = {
      if let Some(dir_prefix) = dir_path.components().next() {
        if let Some(mount_prefix) = mount_point.components().next() {
          // Compare drive letters (usually the prefix)
          format!("{:?}", dir_prefix) == format!("{:?}", mount_prefix)
        } else {
          false
        }
      } else {
        false
      }
    };

    #[cfg(not(any(target_family = "unix", target_os = "windows")))]
    let on_this_disk = dir_path.starts_with(mount_point);

    if on_this_disk {
      let total = disk.total_space();
      let available = disk.available_space();
      let used = total.saturating_sub(available);

      return Some((total, available, used));
    }
  }

  // If we get here, we couldn't find the disk for the path
  None
}

#[cfg(target_family = "unix")]
fn get_owner_name<P: AsRef<Path>>(_path: P, metadata: &std::fs::Metadata) -> Option<String> {
  use std::os::unix::fs::MetadataExt;
  use users::get_user_by_uid;

  let uid = metadata.uid();
  match get_user_by_uid(uid) {
    Some(user) => Some(user.name().to_string_lossy().into_owned()),
    None => Some(format!("<deleted user {}>", uid)), // More explicit format for deleted users
  }
}

#[cfg(target_os = "windows")]
fn get_owner_name<P: AsRef<Path>>(path: P, _metadata: &std::fs::Metadata) -> Option<String> {
  use std::ffi::OsString;
  use std::os::windows::ffi::{OsStrExt, OsStringExt};
  use winapi::ctypes::c_void;
  use winapi::shared::winerror::ERROR_SUCCESS;
  use winapi::um::accctrl::SE_FILE_OBJECT;
  use winapi::um::aclapi::GetNamedSecurityInfoW;
  use winapi::um::securitybaseapi::GetSecurityDescriptorOwner;
  use winapi::um::winbase::LocalFree;
  use winapi::um::winnt::{
    SidTypeAlias, SidTypeDeletedAccount, SidTypeUser, SidTypeWellKnownGroup,
    OWNER_SECURITY_INFORMATION, PSID,
  };

  let path = path.as_ref();
  let path_wide: Vec<u16> = path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect();

  unsafe {
    let mut sid: PSID = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();

    // Get security descriptor
    let status = GetNamedSecurityInfoW(
      path_wide.as_ptr(),
      SE_FILE_OBJECT,
      OWNER_SECURITY_INFORMATION,
      &mut sid,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &mut sd,
    );

    if status != ERROR_SUCCESS {
      eprintln!("GetNamedSecurityInfoW failed with status: {}", status);
      return None;
    }

    // Ensure proper cleanup of security descriptor
    struct SdCleanup(*mut c_void);
    impl Drop for SdCleanup {
      fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
      }
    }
    let _sd_cleanup = SdCleanup(sd);

    // Get the SID owner
    let mut owner: PSID = std::ptr::null_mut();
    let mut owner_defaulted = 0;
    if GetSecurityDescriptorOwner(sd, &mut owner, &mut owner_defaulted) == 0 {
      eprintln!("GetSecurityDescriptorOwner failed");
      return None;
    }

    if owner.is_null() {
      eprintln!("Owner SID is null");
      return None;
    }

    // Convert SID to name
    let mut name_size = 0;
    let mut domain_size = 0;
    let mut sid_type = 0;

    // First call to get buffer sizes
    winapi::um::winbase::LookupAccountSidW(
      std::ptr::null(),
      owner,
      std::ptr::null_mut(),
      &mut name_size,
      std::ptr::null_mut(),
      &mut domain_size,
      &mut sid_type,
    );

    if name_size == 0 {
      eprintln!("LookupAccountSidW failed to get buffer sizes");
      return None;
    }

    // Allocate buffers with proper size
    let mut name_buf = vec![0u16; name_size as usize];
    let mut domain_buf = vec![0u16; domain_size as usize];

    // Second call to get actual data
    if winapi::um::winbase::LookupAccountSidW(
      std::ptr::null(),
      owner,
      name_buf.as_mut_ptr(),
      &mut name_size,
      domain_buf.as_mut_ptr(),
      &mut domain_size,
      &mut sid_type,
    ) == 0
    {
      eprintln!("LookupAccountSidW failed to get account info");
      return None;
    }

    // Accept more SID types - not just users but also groups and aliases
    match sid_type {
      t if t == SidTypeUser || t == SidTypeWellKnownGroup || t == SidTypeAlias => {
        // These are all valid owner types
        let name = OsString::from_wide(&name_buf[0..(name_size - 1) as usize]);
        match name.into_string() {
          Ok(name_str) => Some(name_str),
          Err(_) => {
            eprintln!("Failed to convert name to string");
            None
          }
        }
      }
      t if t == SidTypeDeletedAccount => Some("<deleted account>".to_string()),
      _ => {
        eprintln!("Unsupported SID type: {}", sid_type);
        None
      }
    }
  }
}

#[cfg(not(any(target_family = "unix", target_os = "windows")))]
fn get_owner_name<P: AsRef<Path>>(_path: P, _metadata: &std::fs::Metadata) -> Option<String> {
  None
}
