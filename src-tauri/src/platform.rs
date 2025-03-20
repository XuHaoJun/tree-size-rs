#[allow(unused_imports)]
use std::fs;

use std::path::Path;

#[cfg(target_family = "unix")]
fn get_block_size() -> u64 {
  // All os specific implementations of MetadataExt seem to define a block as 512 bytes
  // https://doc.rust-lang.org/std/os/linux/fs/trait.MetadataExt.html#tymethod.st_blocks
  512
}

type InodeAndDevice = (u64, u64);
type FileTime = (i64, i64, i64);

/// Represents complete information about a filesystem path
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
pub fn get_path_info<P: AsRef<Path>>(
  path: P,
  follow_links: bool,
) -> Option<PathInfo> {
  let path_ref = path.as_ref();

  // First get the metadata
  let (size_bytes, size_allocated_bytes, inode_device, times) = get_metadata(path_ref, follow_links)?;

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
pub fn get_metadata<P: AsRef<Path>>(
  path: P,
  follow_links: bool,
) -> Option<(u64, u64, Option<InodeAndDevice>, FileTime)> {
  // On windows opening the file to get size, file ID and volume can be very
  // expensive because 1) it causes a few system calls, and more importantly 2) it can cause
  // windows defender to scan the file.
  // Therefore we try to avoid doing that for common cases, mainly those of
  // plain files:

  // The idea is to make do with the file size that we get from the OS for
  // free as part of iterating a folder. Therefore we want to make sure that
  // it makes sense to use that free size information:

  // Volume boundaries:
  // The user can ask us not to cross volume boundaries. If the DirEntry is a
  // plain file and not a reparse point or other non-trivial stuff, we assume
  // that the file is located on the same volume as the directory that
  // contains it.

  // File ID:
  // This optimization does deprive us of access to a file ID. As a
  // workaround, we just make one up that hopefully does not collide with real
  // file IDs.
  // Hard links: Unresolved. We don't get inode/file index, so hard links
  // count once for each link. Hopefully they are not too commonly in use on
  // windows.

  // Size:
  // We assume (naively?) that for the common cases the free size info is the
  // same as one would get by doing the expensive thing. Sparse, encrypted and
  // compressed files are not included in the common cases, as one can image
  // there being more than view on their size.

  // Savings in orders of magnitude in terms of time, io and cpu have been
  // observed on hdd, windows 10, some 100Ks files taking up some hundreds of
  // GBs:
  // Consistently opening the file: 30 minutes.
  // With this optimization:         8 sec.

  use std::io;
  use winapi_util::Handle;
  fn handle_from_path_limited(path: &Path) -> io::Result<Handle> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;

    // So, it seems that it does does have to be that expensive to open
    // files to get their info: Avoiding opening the file with the full
    // GENERIC_READ is key:

    // https://docs.microsoft.com/en-us/windows/win32/secauthz/generic-access-rights:
    // "For example, a Windows file object maps the GENERIC_READ bit to the
    // READ_CONTROL and SYNCHRONIZE standard access rights and to the
    // FILE_READ_DATA, FILE_READ_EA, and FILE_READ_ATTRIBUTES
    // object-specific access rights"

    // The flag FILE_READ_DATA seems to be the expensive one, so we'll avoid
    // that, and a most of the other ones. Simply because it seems that we
    // don't need them.

    let file = OpenOptions::new()
      .access_mode(FILE_READ_ATTRIBUTES)
      .open(path)?;
    Ok(Handle::from_file(file))
  }

  fn get_metadata_expensive(path: &Path) -> Option<(u64, u64, Option<InodeAndDevice>, FileTime)> {
    use winapi_util::file::information;
    use filesize::PathExt;

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
        info.last_write_time().unwrap() as i64,
        info.last_access_time().unwrap() as i64,
        info.creation_time().unwrap() as i64,
      ),
    ))
  }

  use std::os::windows::fs::MetadataExt;
  let path = path.as_ref();
  let metadata = if follow_links {
    path.metadata()
  } else {
    path.symlink_metadata()
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
            md.last_write_time() as i64,
            md.last_access_time() as i64,
            md.creation_time() as i64,
          ),
        ))
      } else {
        // For special files (compressed, sparse, etc.), we need the expensive call
        get_metadata_expensive(path)
      }
    }
    _ => get_metadata_expensive(path),
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
    None => Some(format!("<deleted user {}>", uid)) // More explicit format for deleted users
  }
}

#[cfg(target_os = "windows")]
fn get_owner_name<P: AsRef<Path>>(path: P, _metadata: &std::fs::Metadata) -> Option<String> {
  use std::ffi::OsString;
  use std::os::windows::ffi::{OsStringExt, OsStrExt};
  use winapi::um::aclapi::GetNamedSecurityInfoW;
  use winapi::um::winnt::{PSID, SidTypeUser, OWNER_SECURITY_INFORMATION};
  use winapi::um::accctrl::SE_FILE_OBJECT;
  use winapi::shared::winerror::ERROR_SUCCESS;
  use winapi::um::securitybaseapi::GetSecurityDescriptorOwner;
  use winapi::um::winbase::LocalFree;
  
  let path = path.as_ref();
  let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
  
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
      &mut sd
    );
    
    if status != ERROR_SUCCESS {
      return None;
    }

    // Ensure proper cleanup of security descriptor
    struct SdCleanup(*mut std::ffi::c_void);
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
      &mut sid_type
    );
    
    if name_size == 0 {
      return None;
    }
    
    // Allocate buffers with proper size (+1 for null terminator)
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
      &mut sid_type
    ) == 0 {
      return None;
    }
    
    // Ensure we have a user
    if sid_type != SidTypeUser {
      return None;
    }
    
    // Convert to OsString then to String, handling the case where name_size includes null terminator
    let name = OsString::from_wide(&name_buf[0..(name_size - 1) as usize]);
    name.into_string().ok()
  }
}

#[cfg(not(any(target_family = "unix", target_os = "windows")))]
fn get_owner_name<P: AsRef<Path>>(_path: P, _metadata: &std::fs::Metadata) -> Option<String> {
  None
}
