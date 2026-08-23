use std::{
    ffi::CString,
    fs::File,
    io,
    mem::{offset_of, size_of},
    os::fd::AsRawFd,
    path::Path,
};

use thiserror::Error;

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ior(type_: u8, nr: u8, size: usize) -> libc::c_ulong {
    (IOC_READ << IOC_DIRSHIFT
        | (type_ as u32) << IOC_TYPESHIFT
        | (nr as u32) << IOC_NRSHIFT
        | (size as u32) << IOC_SIZESHIFT) as libc::c_ulong
}

const fn iowr(type_: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((IOC_READ | IOC_WRITE) << IOC_DIRSHIFT
        | (type_ as u32) << IOC_TYPESHIFT
        | (nr as u32) << IOC_NRSHIFT
        | (size as u32) << IOC_SIZESHIFT) as libc::c_ulong
}

const FIEMAP_FLAG_SYNC: u32 = 0x0000_0001;
pub const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
pub const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
pub const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
pub const FIEMAP_EXTENT_ENCODED: u32 = 0x0000_0008;
pub const FIEMAP_EXTENT_DATA_ENCRYPTED: u32 = 0x0000_0080;
pub const FIEMAP_EXTENT_NOT_ALIGNED: u32 = 0x0000_0100;
pub const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0000_0200;
pub const FIEMAP_EXTENT_DATA_TAIL: u32 = 0x0000_0400;
pub const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0000_0800;
pub const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
pub const FS_APPEND_FL: u32 = 0x0000_0020;
pub const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
pub const EXT4_INLINE_DATA_FL: u32 = 0x1000_0000;

const FMR_OF_SPECIAL_OWNER: u32 = 0x10;
const FMR_OF_LAST: u32 = 0x20;
const fn fmr_owner(type_: u8, code: u32) -> u64 {
    ((type_ as u64) << 32) | code as u64
}
const FMR_OWN_FREE: u64 = fmr_owner(0, 1);
const FMR_OWN_UNKNOWN: u64 = fmr_owner(0, 2);
const FMR_OWN_METADATA: u64 = fmr_owner(0, 3);
const EXT4_FMR_OWN_FS: u64 = fmr_owner(b'X', 1);
const EXT4_FMR_OWN_LOG: u64 = fmr_owner(b'X', 2);
const EXT4_FMR_OWN_INODES: u64 = fmr_owner(b'X', 5);
const EXT4_FMR_OWN_GDT: u64 = fmr_owner(b'f', 1);
const EXT4_FMR_OWN_RESV_GDT: u64 = fmr_owner(b'f', 2);
const EXT4_FMR_OWN_BLKBM: u64 = fmr_owner(b'f', 3);
const EXT4_FMR_OWN_INOBM: u64 = fmr_owner(b'f', 4);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsMapKind {
    Free,
    Allocated,
    Metadata(MetadataKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataKind {
    FilesystemHeaders,
    Journal,
    FileMetadata,
    GroupDescriptors,
    BlockBitmaps,
    FileBitmaps,
    Reserved,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct FsMapRange {
    pub physical: u64,
    pub length: u64,
    pub kind: FsMapKind,
}

#[derive(Clone, Copy, Debug)]
pub struct FileExtent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub flags: u32,
}

#[derive(Debug, Error)]
pub enum IoctlError {
    #[error("{operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("kernel returned an invalid {0} record count")]
    InvalidCount(&'static str),
    #[error("kernel mapping query did not advance")]
    DidNotAdvance,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Debug)]
struct FiemapRequest<const N: usize> {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [FiemapExtent; N],
}

impl<const N: usize> FiemapRequest<N> {
    fn new(start: u64) -> Self {
        Self {
            start,
            length: u64::MAX - start,
            flags: 0,
            mapped_extents: 0,
            extent_count: N as u32,
            reserved: 0,
            extents: [FiemapExtent::default(); N],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct FsMap {
    device: u32,
    flags: u32,
    physical: u64,
    owner: u64,
    offset: u64,
    length: u64,
    reserved: [u64; 3],
}

#[repr(C)]
#[derive(Debug)]
struct FsMapRequest<const N: usize> {
    iflags: u32,
    oflags: u32,
    count: u32,
    entries: u32,
    reserved: [u64; 6],
    keys: [FsMap; 2],
    records: [FsMap; N],
}

impl<const N: usize> FsMapRequest<N> {
    fn new(low: FsMap) -> Self {
        Self {
            iflags: 0,
            oflags: 0,
            count: N as u32,
            entries: 0,
            reserved: [0; 6],
            keys: [
                low,
                FsMap {
                    device: u32::MAX,
                    flags: u32::MAX,
                    physical: u64::MAX,
                    owner: u64::MAX,
                    offset: u64::MAX,
                    length: u64::MAX,
                    reserved: [0; 3],
                },
            ],
            records: [FsMap::default(); N],
        }
    }
}

pub fn fiemap(file: &File) -> Result<Vec<FileExtent>, IoctlError> {
    const BATCH: usize = 256;
    const FIEMAP_HEADER_SIZE: usize = offset_of!(FiemapRequest<1>, extents);
    let request_code = iowr(b'f', 11, FIEMAP_HEADER_SIZE);
    let mut start = 0u64;
    let mut result = Vec::new();

    loop {
        let mut request = FiemapRequest::<BATCH>::new(start);
        debug_assert_eq!(request.flags & FIEMAP_FLAG_SYNC, 0);
        // SAFETY: request is a writable C-compatible buffer whose encoded
        // header size matches struct fiemap; the trailing extent array has
        // exactly extent_count entries.
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), request_code, &mut request) };
        if rc < 0 {
            return Err(IoctlError::Io {
                operation: "FS_IOC_FIEMAP",
                source: io::Error::last_os_error(),
            });
        }
        let count = request.mapped_extents as usize;
        if count > BATCH {
            return Err(IoctlError::InvalidCount("FIEMAP"));
        }
        if count == 0 {
            break;
        }

        for extent in &request.extents[..count] {
            result.push(FileExtent {
                logical: extent.logical,
                physical: extent.physical,
                length: extent.length,
                flags: extent.flags,
            });
        }

        let last = request.extents[count - 1];
        if last.flags & FIEMAP_EXTENT_LAST != 0 {
            break;
        }
        let next = last.logical.saturating_add(last.length);
        if next <= start {
            return Err(IoctlError::DidNotAdvance);
        }
        start = next;
    }
    Ok(result)
}

pub fn fsmap(file: &File) -> Result<Vec<FsMapRange>, IoctlError> {
    const BATCH: usize = 256;
    const FSMAP_HEADER_SIZE: usize = offset_of!(FsMapRequest<1>, records);
    let request_code = iowr(b'X', 59, FSMAP_HEADER_SIZE);
    let mut low = FsMap::default();
    let mut result = Vec::new();

    loop {
        let mut request = FsMapRequest::<BATCH>::new(low);
        // SAFETY: request is a writable C-compatible fsmap_head followed by
        // count records. All reserved fields are zero as required by UAPI.
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), request_code, &mut request) };
        if rc < 0 {
            return Err(IoctlError::Io {
                operation: "FS_IOC_GETFSMAP",
                source: io::Error::last_os_error(),
            });
        }
        let count = request.entries as usize;
        if count > BATCH {
            return Err(IoctlError::InvalidCount("FSMAP"));
        }
        if count == 0 {
            break;
        }

        for record in &request.records[..count] {
            let kind = if record.flags & FMR_OF_SPECIAL_OWNER == 0 {
                FsMapKind::Allocated
            } else if record.owner == FMR_OWN_FREE {
                FsMapKind::Free
            } else {
                match record.owner {
                    FMR_OWN_UNKNOWN => FsMapKind::Allocated,
                    EXT4_FMR_OWN_FS => FsMapKind::Metadata(MetadataKind::FilesystemHeaders),
                    EXT4_FMR_OWN_LOG => FsMapKind::Metadata(MetadataKind::Journal),
                    EXT4_FMR_OWN_INODES => FsMapKind::Metadata(MetadataKind::FileMetadata),
                    EXT4_FMR_OWN_GDT | EXT4_FMR_OWN_RESV_GDT => {
                        FsMapKind::Metadata(MetadataKind::GroupDescriptors)
                    }
                    EXT4_FMR_OWN_BLKBM => FsMapKind::Metadata(MetadataKind::BlockBitmaps),
                    EXT4_FMR_OWN_INOBM => FsMapKind::Metadata(MetadataKind::FileBitmaps),
                    FMR_OWN_METADATA => FsMapKind::Metadata(MetadataKind::Other),
                    _ => FsMapKind::Metadata(MetadataKind::Reserved),
                }
            };
            result.push(FsMapRange {
                physical: record.physical,
                length: record.length,
                kind,
            });
        }

        let last = request.records[count - 1];
        if last.flags & FMR_OF_LAST != 0 {
            break;
        }
        let next = last.physical.saturating_add(last.length);
        let previous_end = low.physical.saturating_add(low.length);
        if next <= previous_end {
            return Err(IoctlError::DidNotAdvance);
        }
        // The UAPI explicitly requires copying the last record verbatim into
        // the next low key. The filesystem advances by fmr_length internally.
        low = last;
    }
    Ok(result)
}

pub fn file_flags(file: &File) -> io::Result<u32> {
    let request_code = ior(b'f', 1, size_of::<libc::c_long>());
    let mut flags: libc::c_long = 0;
    // SAFETY: flags points to writable storage of the size encoded in the
    // request. FS_IOC_GETFLAGS does not mutate the file.
    if unsafe { libc::ioctl(file.as_raw_fd(), request_code, &mut flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags as u32)
}

pub fn mount_id(path: &Path) -> io::Result<u64> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: statx is a plain C output structure, fully initialized by the
    // syscall on success.
    let mut stat = unsafe { std::mem::zeroed::<libc::statx>() };
    // SAFETY: path is NUL terminated and stat points to writable storage.
    let rc = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(stat.stx_mnt_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapi_layouts_match_linux_headers() {
        assert_eq!(size_of::<FiemapExtent>(), 56);
        assert_eq!(offset_of!(FiemapRequest<1>, extents), 32);
        assert_eq!(size_of::<FsMap>(), 64);
        assert_eq!(offset_of!(FsMapRequest<1>, records), 192);
    }

    #[test]
    fn ioctl_numbers_match_x86_64_linux_uapi() {
        assert_eq!(iowr(b'f', 11, 32), 0xC020_660B);
        assert_eq!(iowr(b'X', 59, 192), 0xC0C0_583B);
    }
}
