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
    AllocationTables,
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
    #[error("kernel returned an invalid filesystem block size {0}")]
    InvalidBlockSize(libc::c_int),
    #[error("file is too large for the legacy FIBMAP interface")]
    FileTooLargeForFibmap,
    #[error("EXT4_IOC_MOVE_EXT returned an invalid moved length {0}")]
    InvalidMovedLength(u64),
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct MoveExtent {
    reserved: i32,
    donor_fd: u32,
    orig_start: u64,
    donor_start: u64,
    len: u64,
    moved_len: u64,
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
    fiemap_with_flags(file, 0)
}

pub fn fiemap_sync(file: &File) -> Result<Vec<FileExtent>, IoctlError> {
    fiemap_with_flags(file, FIEMAP_FLAG_SYNC)
}

fn fiemap_with_flags(file: &File, flags: u32) -> Result<Vec<FileExtent>, IoctlError> {
    const BATCH: usize = 256;
    const FIEMAP_HEADER_SIZE: usize = offset_of!(FiemapRequest<1>, extents);
    let request_code = iowr(b'f', 11, FIEMAP_HEADER_SIZE);
    let mut start = 0u64;
    let mut result = Vec::new();

    loop {
        let mut request = FiemapRequest::<BATCH>::new(start);
        request.flags = flags;
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

pub fn filesystem_block_size(file: &File) -> Result<u64, IoctlError> {
    const FIGETBSZ: libc::c_ulong = 2;
    let mut block_size: libc::c_int = 0;
    // SAFETY: block_size is writable storage with the type FIGETBSZ expects.
    if unsafe { libc::ioctl(file.as_raw_fd(), FIGETBSZ, &mut block_size) } < 0 {
        return Err(IoctlError::Io {
            operation: "FIGETBSZ",
            source: io::Error::last_os_error(),
        });
    }
    if block_size <= 0 {
        return Err(IoctlError::InvalidBlockSize(block_size));
    }
    Ok(block_size as u64)
}

pub fn move_extents(
    original: &File,
    donor: &File,
    logical_block: u64,
    block_count: u64,
) -> Result<u64, IoctlError> {
    let donor_fd = u32::try_from(donor.as_raw_fd()).map_err(|_| IoctlError::Io {
        operation: "EXT4_IOC_MOVE_EXT donor fd",
        source: io::Error::from_raw_os_error(libc::EBADF),
    })?;
    let mut request = MoveExtent {
        donor_fd,
        orig_start: logical_block,
        donor_start: logical_block,
        len: block_count,
        ..MoveExtent::default()
    };
    let request_code = iowr(b'f', 15, size_of::<MoveExtent>());
    // SAFETY: request matches the Linux move_extent UAPI and both descriptors
    // remain open for the duration of the ioctl.
    if unsafe { libc::ioctl(original.as_raw_fd(), request_code, &mut request) } < 0 {
        return Err(IoctlError::Io {
            operation: "EXT4_IOC_MOVE_EXT",
            source: io::Error::last_os_error(),
        });
    }
    if request.moved_len == 0 || request.moved_len > block_count {
        return Err(IoctlError::InvalidMovedLength(request.moved_len));
    }
    Ok(request.moved_len)
}

/// Return FAT-family file extents using FIEMAP when a kernel implements it,
/// with the older FIBMAP interface as the compatibility path.
///
/// Linux currently exposes `bmap` rather than `fiemap` for its FAT and exFAT
/// drivers. FIBMAP is intentionally capability-gated by the VFS, so callers
/// must surface `EPERM` rather than treating it as an empty file.
pub fn fat_file_extents(file: &File, logical_bytes: u64) -> Result<Vec<FileExtent>, IoctlError> {
    match fiemap(file) {
        Ok(extents) => Ok(extents),
        Err(IoctlError::Io { source, .. })
            if matches!(
                source.raw_os_error(),
                Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY)
            ) =>
        {
            fibmap(file, logical_bytes)
        }
        Err(error) => Err(error),
    }
}

fn fibmap(file: &File, logical_bytes: u64) -> Result<Vec<FileExtent>, IoctlError> {
    // _IO(0x00, 2) and _IO(0x00, 1); direction and encoded size are both zero.
    const FIBMAP: libc::c_ulong = 1;
    let block_size = filesystem_block_size(file)?;
    let block_count = logical_bytes.div_ceil(block_size);
    if block_count > libc::c_int::MAX as u64 {
        return Err(IoctlError::FileTooLargeForFibmap);
    }

    let mut result: Vec<FileExtent> = Vec::new();
    for logical_block in 0..block_count {
        let mut physical_block = logical_block as libc::c_int;
        // SAFETY: physical_block is an in/out integer as required by FIBMAP.
        if unsafe { libc::ioctl(file.as_raw_fd(), FIBMAP, &mut physical_block) } < 0 {
            return Err(IoctlError::Io {
                operation: "FIBMAP (requires CAP_SYS_RAWIO)",
                source: io::Error::last_os_error(),
            });
        }
        if physical_block <= 0 {
            continue;
        }
        let logical = logical_block.saturating_mul(block_size);
        let physical = (physical_block as u64).saturating_mul(block_size);
        if let Some(last) = result.last_mut()
            && last.logical.saturating_add(last.length) == logical
            && last.physical.saturating_add(last.length) == physical
        {
            last.length = last.length.saturating_add(block_size);
        } else {
            result.push(FileExtent {
                logical,
                physical,
                length: block_size,
                flags: 0,
            });
        }
    }
    if let Some(last) = result.last_mut() {
        last.flags |= FIEMAP_EXTENT_LAST;
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
        assert_eq!(size_of::<MoveExtent>(), 40);
    }

    #[test]
    fn ioctl_numbers_match_x86_64_linux_uapi() {
        assert_eq!(iowr(b'f', 11, 32), 0xC020_660B);
        assert_eq!(iowr(b'X', 59, 192), 0xC0C0_583B);
    }
}
