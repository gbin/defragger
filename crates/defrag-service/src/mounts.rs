use std::{ffi::CString, fs, path::PathBuf};

use defrag_domain::{SupportStatus, Volume, VolumeId};

use crate::{FilesystemBackend, ServiceError};

#[derive(Debug, Eq, PartialEq)]
struct MountInfo {
    mount_id: u64,
    parent_mount_id: u64,
    major: u32,
    minor: u32,
    mount_point: PathBuf,
    options: Vec<String>,
    filesystem: String,
    source: String,
}

pub fn discover(
    backends: &[std::sync::Arc<dyn FilesystemBackend>],
) -> Result<Vec<Volume>, ServiceError> {
    let contents = fs::read_to_string("/proc/self/mountinfo")?;
    let mut volumes = Vec::new();
    for line in contents.lines() {
        let Some(info) = parse_line(line) else {
            continue;
        };
        if !is_interesting(&info) {
            continue;
        }
        let (capacity, free) = statvfs(&info.mount_point).unwrap_or((0, 0));
        let mut volume = Volume {
            id: VolumeId(info.mount_id),
            mount_id: info.mount_id,
            parent_mount_id: info.parent_mount_id,
            device_major: info.major,
            device_minor: info.minor,
            mount_point: info.mount_point,
            source: info.source,
            filesystem: info.filesystem,
            read_only: info.options.iter().any(|option| option == "ro"),
            capacity_bytes: capacity,
            used_bytes: capacity.saturating_sub(free),
            free_bytes: free,
            support: SupportStatus::Unsupported {
                reason: "No filesystem backend is installed".into(),
            },
        };
        for backend in backends {
            let support = backend.probe(&volume);
            if matches!(support, SupportStatus::ReadOnly) {
                volume.support = support;
                break;
            }
        }
        volumes.push(volume);
    }
    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    Ok(volumes)
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
    let _root = left_fields.next()?;
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
    }
}
