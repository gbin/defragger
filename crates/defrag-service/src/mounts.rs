use std::{
    collections::BTreeMap,
    ffi::CString,
    fs, io,
    os::unix::fs::{FileExt, FileTypeExt, MetadataExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use defrag_domain::{MountState, SupportStatus, Volume, VolumeId};

use crate::{FilesystemBackend, ServiceError};

#[derive(Debug, Eq, PartialEq)]
struct MountInfo {
    mount_id: u64,
    parent_mount_id: u64,
    major: u32,
    minor: u32,
    root: PathBuf,
    mount_point: PathBuf,
    options: Vec<String>,
    filesystem: String,
    source: String,
}

pub fn discover(
    backends: &[std::sync::Arc<dyn FilesystemBackend>],
) -> Result<Vec<Volume>, ServiceError> {
    let mounts: Vec<_> = fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .filter_map(parse_line)
        .filter(is_interesting)
        .collect();
    let mut devices = block_devices();
    for info in &mounts {
        devices
            .entry((info.major, info.minor))
            .or_insert_with(|| DeviceInfo {
                source: info.source.clone(),
                filesystem: info.filesystem.clone(),
                capacity: 0,
                label: None,
                uuid: None,
            });
    }

    let mut volumes = Vec::new();
    for ((major, minor), device) in devices {
        let mounted = mounts
            .iter()
            .filter(|mount| mount.major == major && mount.minor == minor)
            .min_by_key(|mount| {
                (
                    mount.root.as_path() != std::path::Path::new("/"),
                    mount.mount_point.clone(),
                )
            });
        let (mount_id, parent_mount_id, mount_point, source, filesystem, read_only) =
            if let Some(info) = mounted {
                (
                    Some(info.mount_id),
                    Some(info.parent_mount_id),
                    Some(info.mount_point.clone()),
                    info.source.clone(),
                    info.filesystem.clone(),
                    info.options.iter().any(|option| option == "ro"),
                )
            } else {
                (None, None, None, device.source, device.filesystem, false)
            };
        if filesystem.is_empty() {
            continue;
        }
        let (capacity, free) = mount_point
            .as_deref()
            .and_then(statvfs)
            .map_or((device.capacity, None), |(capacity, free)| {
                (capacity, Some(free))
            });
        let mount_state = match (mount_point.is_some(), read_only) {
            (false, _) => MountState::Unmounted,
            (true, true) => MountState::MountedReadOnly,
            (true, false) => MountState::MountedReadWrite,
        };
        let mut volume = Volume {
            id: device_volume_id(major, minor),
            mount_id,
            parent_mount_id,
            device_major: major,
            device_minor: minor,
            mount_point,
            source,
            filesystem,
            label: device.label,
            uuid: device.uuid,
            mount_state,
            read_only,
            capacity_bytes: capacity,
            used_bytes: free.map(|free| capacity.saturating_sub(free)),
            free_bytes: free,
            support: SupportStatus::Unsupported {
                reason: "No filesystem backend is installed".into(),
            },
        };
        for backend in backends {
            let support = backend.probe(&volume);
            if matches!(
                support,
                SupportStatus::ReadOnly | SupportStatus::Defragmentable
            ) {
                volume.support = support;
                break;
            }
        }
        volumes.push(volume);
    }
    volumes.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(volumes)
}

#[derive(Debug)]
struct DeviceInfo {
    source: String,
    filesystem: String,
    capacity: u64,
    label: Option<String>,
    uuid: Option<String>,
}

fn block_devices() -> BTreeMap<(u32, u32), DeviceInfo> {
    let mut devices = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/sys/class/block") else {
        return devices;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Some((major, minor)) = fs::read_to_string(path.join("dev"))
            .ok()
            .and_then(|value| parse_device_number(value.trim()))
        else {
            continue;
        };
        let properties = udev_properties(major, minor);
        if properties
            .get("ID_FS_USAGE")
            .is_none_or(|usage| usage != "filesystem")
        {
            continue;
        }
        let Some(filesystem) = properties.get("ID_FS_TYPE").cloned() else {
            continue;
        };
        let sectors = fs::read_to_string(path.join("size"))
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        devices.insert(
            (major, minor),
            DeviceInfo {
                source: format!("/dev/{name}"),
                filesystem,
                capacity: sectors.saturating_mul(512),
                label: properties.get("ID_FS_LABEL").cloned(),
                uuid: properties.get("ID_FS_UUID").cloned(),
            },
        );
    }
    devices
}

fn udev_properties(major: u32, minor: u32) -> BTreeMap<String, String> {
    let path = format!("/run/udev/data/b{major}:{minor}");
    fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|contents| contents.lines().map(str::to_owned).collect::<Vec<_>>())
        .filter_map(|line| {
            line.strip_prefix("E:")?
                .split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

fn parse_device_number(value: &str) -> Option<(u32, u32)> {
    let (major, minor) = value.split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn device_volume_id(major: u32, minor: u32) -> VolumeId {
    VolumeId((u64::from(major) << 32) | u64::from(minor))
}

fn is_interesting(info: &MountInfo) -> bool {
    if matches!(
        info.filesystem.as_str(),
        "proc"
            | "sysfs"
            | "tmpfs"
            | "devtmpfs"
            | "devpts"
            | "cgroup"
            | "cgroup2"
            | "overlay"
            | "squashfs"
            | "tracefs"
            | "debugfs"
            | "securityfs"
            | "pstore"
            | "efivarfs"
            | "mqueue"
            | "hugetlbfs"
            | "fusectl"
            | "configfs"
            | "autofs"
    ) {
        return false;
    }
    info.source.starts_with("/dev/") || info.filesystem == "ext4"
}

fn parse_line(line: &str) -> Option<MountInfo> {
    let (left, right) = line.split_once(" - ")?;
    let mut left_fields = left.split_whitespace();
    let mount_id = left_fields.next()?.parse().ok()?;
    let parent_mount_id = left_fields.next()?.parse().ok()?;
    let (major, minor) = left_fields.next()?.split_once(':')?;
    let major = major.parse().ok()?;
    let minor = minor.parse().ok()?;
    let root = PathBuf::from(unescape(left_fields.next()?));
    let mount_point = PathBuf::from(unescape(left_fields.next()?));
    let options = left_fields.next()?.split(',').map(str::to_owned).collect();

    let mut right_fields = right.split_whitespace();
    let filesystem = right_fields.next()?.to_owned();
    let source = unescape(right_fields.next()?);
    Some(MountInfo {
        mount_id,
        parent_mount_id,
        major,
        minor,
        root,
        mount_point,
        options,
        filesystem,
        source,
    })
}

fn unescape(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = &value[index + 1..index + 4];
            if let Ok(decoded) = u8::from_str_radix(octal, 8) {
                result.push(decoded as char);
                index += 4;
                continue;
            }
        }
        result.push(bytes[index] as char);
        index += 1;
    }
    result
}

fn statvfs(path: &std::path::Path) -> Option<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: stat is initialized by statvfs when the C path is valid.
    let mut stat = unsafe { std::mem::zeroed::<libc::statvfs>() };
    // SAFETY: path is NUL terminated and stat points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let fragment_size = stat.f_frsize;
    Some((
        stat.f_blocks.saturating_mul(fragment_size),
        stat.f_bavail.saturating_mul(fragment_size),
    ))
}

static NEXT_PRIVATE_MOUNT: AtomicU64 = AtomicU64::new(1);

pub(crate) fn unmount(volume: &Volume) -> Result<(), ServiceError> {
    if volume.mount_state == MountState::Unmounted {
        return Ok(());
    }
    let mount_point = volume.mount_point.as_ref().ok_or_else(|| {
        ServiceError::UnmountUnavailable("the volume has no mount point".to_owned())
    })?;
    let path = CString::new(mount_point.as_os_str().as_encoded_bytes()).map_err(|_| {
        ServiceError::UnmountUnavailable("the mount point contains a NUL byte".to_owned())
    })?;
    // SAFETY: path is a valid NUL-terminated mount point. No force or lazy
    // flags are used, so the kernel rejects busy volumes instead of detaching
    // them while an application still has files open.
    if unsafe { libc::umount(path.as_ptr()) } != 0 {
        return Err(operation_error(
            &format!("could not unmount {}", mount_point.display()),
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

pub(crate) struct JobMount {
    pub(crate) volume: Volume,
    temporary_path: Option<PathBuf>,
}

impl Drop for JobMount {
    fn drop(&mut self) {
        let Some(path) = self.temporary_path.take() else {
            return;
        };
        if let Ok(path_c) = CString::new(path.as_os_str().as_encoded_bytes()) {
            // SAFETY: path_c is a valid NUL-terminated mount point. MNT_DETACH
            // guarantees cleanup even if a failed job left an fd open.
            unsafe { libc::umount2(path_c.as_ptr(), libc::MNT_DETACH) };
        }
        let _ = fs::remove_dir(path);
    }
}

pub(crate) fn mount_for_job(volume: &Volume, writable: bool) -> Result<JobMount, ServiceError> {
    if writable && volume.filesystem == "ext4" {
        validate_ext4_writable(volume)?;
    }
    if volume.mount_state != MountState::Unmounted {
        if writable && volume.mount_state != MountState::MountedReadWrite {
            return Err(ServiceError::Io(std::io::Error::new(
                std::io::ErrorKind::ReadOnlyFilesystem,
                "the selected filesystem is mounted read-only",
            )));
        }
        return Ok(JobMount {
            volume: volume.clone(),
            temporary_path: None,
        });
    }

    validate_block_device(volume)?;
    if !writable && volume.filesystem == "ext4" {
        validate_ext4_clean(volume)?;
    }
    // SAFETY: this job already runs on its own worker thread. A new mount
    // namespace prevents the temporary mount from appearing in the desktop.
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } < 0 {
        return Err(operation_error(
            "could not create the job's private mount namespace",
            io::Error::last_os_error(),
        ));
    }
    let root = CString::new("/").expect("literal contains no NUL");
    // SAFETY: a null source/fstype is required for propagation-only mounts.
    if unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong,
            std::ptr::null(),
        )
    } < 0
    {
        return Err(operation_error(
            "could not make mounts private inside the job namespace",
            io::Error::last_os_error(),
        ));
    }

    let serial = NEXT_PRIVATE_MOUNT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "defragger-{}-{}-{serial}",
        std::process::id(),
        volume.id.0
    ));
    fs::create_dir(&path).map_err(|error| {
        operation_error(
            &format!("could not create private mount point {}", path.display()),
            error,
        )
    })?;
    let cstring = |bytes: &[u8]| {
        CString::new(bytes).map_err(|error| {
            ServiceError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
        })
    };
    let source = cstring(volume.source.as_bytes())?;
    let target = cstring(path.as_os_str().as_encoded_bytes())?;
    let filesystem = cstring(volume.filesystem.as_bytes())?;
    // nodev/nosuid/noexec are VFS flags, not filesystem option text. Passing
    // them in `data` makes ext4's option parser reject the mount with EINVAL.
    let options = (!writable && volume.filesystem == "ext4")
        .then(|| cstring(b"noload"))
        .transpose()?;
    let options_pointer = options
        .as_ref()
        .map_or(std::ptr::null(), |options| options.as_ptr().cast());
    let flags = libc::MS_NODEV
        | libc::MS_NOSUID
        | libc::MS_NOEXEC
        | if writable { 0 } else { libc::MS_RDONLY };
    // SAFETY: all strings are NUL terminated; mount validates the device and
    // filesystem before making it visible inside this namespace.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            filesystem.as_ptr(),
            flags as libc::c_ulong,
            options_pointer,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        let _ = fs::remove_dir(&path);
        return Err(operation_error(
            &format!(
                "could not mount {} as {} {} at {}",
                volume.source,
                volume.filesystem,
                if writable { "read-write" } else { "read-only" },
                path.display()
            ),
            error,
        ));
    }

    let mut mounted = JobMount {
        volume: volume.clone(),
        temporary_path: Some(path.clone()),
    };
    mounted.volume.mount_point = Some(path.clone());
    mounted.volume.mount_id = Some(crate::linux::mount_id(&path).map_err(|error| {
        operation_error(
            &format!("could not identify private mount {}", path.display()),
            error,
        )
    })?);
    mounted.volume.parent_mount_id = None;
    mounted.volume.read_only = !writable;
    if let Some((capacity, free)) = statvfs(&path) {
        mounted.volume.capacity_bytes = capacity;
        mounted.volume.free_bytes = Some(free);
        mounted.volume.used_bytes = Some(capacity.saturating_sub(free));
    }
    Ok(mounted)
}

pub(crate) fn mount_for_analysis(
    volume: &Volume,
    allow_journal_recovery: bool,
) -> Result<JobMount, ServiceError> {
    let needs_recovery = volume.mount_state == MountState::Unmounted
        && volume.filesystem == "ext4"
        && ext4_needs_recovery(volume)?;
    if needs_recovery && !allow_journal_recovery {
        return Err(ext4_recovery_required());
    }
    mount_for_job(volume, needs_recovery)
}

fn operation_error(operation: &str, error: io::Error) -> ServiceError {
    ServiceError::Io(io::Error::new(
        error.kind(),
        format!("{operation}: {error}"),
    ))
}

fn validate_block_device(volume: &Volume) -> Result<(), ServiceError> {
    let metadata = fs::metadata(&volume.source)?;
    if !metadata.file_type().is_block_device()
        || libc::major(metadata.rdev()) != volume.device_major
        || libc::minor(metadata.rdev()) != volume.device_minor
    {
        return Err(ServiceError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the selected block device no longer matches the discovered volume",
        )));
    }
    Ok(())
}

pub(crate) fn ext4_needs_recovery(volume: &Volume) -> Result<bool, ServiceError> {
    if volume.mount_state != MountState::Unmounted || volume.filesystem != "ext4" {
        return Ok(false);
    }
    validate_block_device(volume)?;
    const EXT4_SUPER_OFFSET: u64 = 1024;
    let file = fs::File::open(&volume.source)?;
    let mut superblock = [0u8; 1024];
    file.read_exact_at(&mut superblock, EXT4_SUPER_OFFSET)?;
    let health = ext4_superblock_health(&superblock)?;
    if health.has_errors {
        return Err(ext4_filesystem_has_errors());
    }
    Ok(health.needs_recovery)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ext4Health {
    needs_recovery: bool,
    has_errors: bool,
}

fn ext4_superblock_health(superblock: &[u8; 1024]) -> Result<Ext4Health, ServiceError> {
    const EXT4_MAGIC_OFFSET: usize = 0x38;
    const EXT4_STATE_OFFSET: usize = 0x3a;
    const EXT4_ERROR_FS: u16 = 0x0002;
    const EXT4_FEATURE_INCOMPAT_OFFSET: usize = 0x60;
    const EXT4_FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;
    let magic = u16::from_le_bytes([
        superblock[EXT4_MAGIC_OFFSET],
        superblock[EXT4_MAGIC_OFFSET + 1],
    ]);
    let incompat = u32::from_le_bytes(
        superblock[EXT4_FEATURE_INCOMPAT_OFFSET..EXT4_FEATURE_INCOMPAT_OFFSET + 4]
            .try_into()
            .expect("fixed-size slice"),
    );
    if magic != 0xef53 {
        return Err(ServiceError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the selected device no longer contains ext4",
        )));
    }
    let state = u16::from_le_bytes([
        superblock[EXT4_STATE_OFFSET],
        superblock[EXT4_STATE_OFFSET + 1],
    ]);
    Ok(Ext4Health {
        needs_recovery: incompat & EXT4_FEATURE_INCOMPAT_RECOVER != 0,
        has_errors: state & EXT4_ERROR_FS != 0,
    })
}

pub(crate) fn validate_ext4_writable(volume: &Volume) -> Result<(), ServiceError> {
    if volume.filesystem != "ext4" {
        return Ok(());
    }
    validate_block_device(volume)?;
    const EXT4_SUPER_OFFSET: u64 = 1024;
    let file = fs::File::open(&volume.source)?;
    let mut superblock = [0u8; 1024];
    file.read_exact_at(&mut superblock, EXT4_SUPER_OFFSET)?;
    if ext4_superblock_health(&superblock)?.has_errors {
        return Err(ext4_filesystem_has_errors());
    }
    Ok(())
}

fn ext4_filesystem_has_errors() -> ServiceError {
    ServiceError::UnsafeFilesystem(
        "ext4 is marked as containing errors; keep it unmounted and run e2fsck before analysis or defragmentation"
            .to_owned(),
    )
}

fn validate_ext4_clean(volume: &Volume) -> Result<(), ServiceError> {
    if ext4_needs_recovery(volume)? {
        return Err(ext4_recovery_required());
    }
    Ok(())
}

fn ext4_recovery_required() -> ServiceError {
    ServiceError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "ext4 journal recovery is required; modification authorization is needed to replay it",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mountinfo_and_decodes_paths() {
        let line = "42 35 8:2 / /media/My\\040Disk rw,nosuid shared:7 - ext4 /dev/sda2 rw";
        let info = parse_line(line).unwrap();
        assert_eq!(info.mount_id, 42);
        assert_eq!(info.mount_point, PathBuf::from("/media/My Disk"));
        assert_eq!(info.filesystem, "ext4");
        assert_eq!(info.source, "/dev/sda2");
        assert_eq!(info.root, PathBuf::from("/"));
    }

    #[test]
    fn volume_ids_are_device_based() {
        assert_eq!(device_volume_id(8, 2), VolumeId((8_u64 << 32) | 2));
    }

    #[test]
    fn detects_when_an_ext4_superblock_needs_journal_recovery() {
        let mut superblock = [0_u8; 1024];
        superblock[0x38..0x3a].copy_from_slice(&0xef53_u16.to_le_bytes());
        assert_eq!(
            ext4_superblock_health(&superblock).unwrap(),
            Ext4Health {
                needs_recovery: false,
                has_errors: false
            }
        );

        superblock[0x60..0x64].copy_from_slice(&0x0004_u32.to_le_bytes());
        superblock[0x3a..0x3c].copy_from_slice(&0x0002_u16.to_le_bytes());
        assert_eq!(
            ext4_superblock_health(&superblock).unwrap(),
            Ext4Health {
                needs_recovery: true,
                has_errors: true
            }
        );
    }
}
