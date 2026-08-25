use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::{fs::OpenOptionsExt, prelude::FileExt},
    path::{Path, PathBuf},
};

use fat::{FatFs, FatVariant, FileId, Geometry};

use crate::{
    ServiceError,
    linux::{FsMapKind, FsMapRange, MetadataKind},
};

const FIXED_ROOT: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClassicKind {
    Fat12,
    Fat16,
    Fat32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawFile {
    pub path: PathBuf,
    pub size: u64,
    pub attributes: u32,
    pub entry_offset: u64,
    pub first_cluster: u32,
    pub chain: Vec<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct ClassicSnapshot {
    pub kind: ClassicKind,
    pub geometry: Geometry,
    pub boot: [u8; 512],
    pub fat: Vec<u8>,
    pub files: Vec<RawFile>,
    pub directory_clusters: HashSet<u32>,
    pub free_clusters: Vec<u32>,
    pub bad_clusters: HashSet<u32>,
    pub ranges: Vec<FsMapRange>,
    pub writable_issues: Vec<String>,
}

impl ClassicSnapshot {
    pub(crate) fn read(source: &str) -> Result<Self, ServiceError> {
        let raw = File::open(source)?;
        Self::read_handle(&raw)
    }

    pub(crate) fn read_handle(handle: &File) -> Result<Self, ServiceError> {
        let mut raw = handle.try_clone()?;
        let mut boot = [0u8; 512];
        raw.read_exact(&mut boot)?;
        raw.seek(SeekFrom::Start(0))?;
        let reader = FatFs::open(raw.try_clone()?).map_err(fat_error)?;
        let geometry = reader.geometry().clone();
        let kind = match reader.variant() {
            FatVariant::Fat12 => ClassicKind::Fat12,
            FatVariant::Fat16 => ClassicKind::Fat16,
            FatVariant::Fat32 => ClassicKind::Fat32,
            FatVariant::ExFat => {
                return Err(ServiceError::UnsafeFilesystem(
                    "exFAT is not a classic FAT volume".to_owned(),
                ));
            }
        };
        let fat_len = u64::from(geometry.fat_size_sectors)
            .checked_mul(u64::from(geometry.bytes_per_sector))
            .ok_or_else(|| unsafe_fs("FAT byte length overflows"))?;
        let fat_len_usize = usize::try_from(fat_len)
            .map_err(|_| unsafe_fs("FAT is too large to validate in memory"))?;
        let mut fat = vec![0; fat_len_usize];
        read_exact_at(&mut raw, geometry.fat_start, &mut fat)?;

        let mut writable_issues = Vec::new();
        if geometry.num_fats < 1 || geometry.num_fats > 2 {
            writable_issues.push(format!(
                "unsupported number of FAT copies: {}",
                geometry.num_fats
            ));
        }
        for copy in 1..geometry.num_fats {
            let mut mirror = vec![0; fat_len_usize];
            read_exact_at(
                &mut raw,
                geometry
                    .fat_start
                    .saturating_add(u64::from(copy).saturating_mul(fat_len)),
                &mut mirror,
            )?;
            if mirror != fat {
                writable_issues.push(format!("FAT copy {} differs from FAT1", copy + 1));
            }
        }
        if kind == ClassicKind::Fat32 {
            let ext_flags = u16::from_le_bytes([boot[40], boot[41]]);
            if ext_flags & 0x0080 != 0 {
                writable_issues
                    .push("FAT32 mirroring is disabled and an active FAT is selected".to_owned());
            }
        }
        let status = fat_entry(&fat, kind, 1).unwrap_or(0);
        match kind {
            ClassicKind::Fat12 => {}
            ClassicKind::Fat16 => {
                if status & (1 << 15) == 0 {
                    writable_issues.push("the FAT clean-shutdown bit is clear".to_owned());
                }
                if status & (1 << 14) == 0 {
                    writable_issues.push("the FAT hard-error bit is clear".to_owned());
                }
            }
            ClassicKind::Fat32 => {
                if status & (1 << 27) == 0 {
                    writable_issues.push("the FAT clean-shutdown bit is clear".to_owned());
                }
                if status & (1 << 26) == 0 {
                    writable_issues.push("the FAT hard-error bit is clear".to_owned());
                }
            }
        }

        let mut files = Vec::new();
        let mut directory_clusters = HashSet::new();
        let mut ownership: HashMap<u32, String> = HashMap::new();
        let mut seen_directories = HashSet::new();
        let root_chain = if kind == ClassicKind::Fat32 {
            chain(
                &fat,
                kind,
                geometry.root_cluster,
                geometry.count_of_clusters,
            )?
        } else {
            Vec::new()
        };
        register_chain(
            &root_chain,
            "directory /",
            &mut ownership,
            &mut writable_issues,
        );
        directory_clusters.extend(root_chain);
        walk_directory(
            &reader,
            &geometry,
            kind,
            &fat,
            reader.root(),
            Path::new("/"),
            &mut seen_directories,
            &mut directory_clusters,
            &mut ownership,
            &mut files,
            &mut writable_issues,
        )?;

        let mut free_clusters = Vec::new();
        let mut bad_clusters = HashSet::new();
        for cluster in 2..geometry.count_of_clusters.saturating_add(2) {
            let value = fat_entry(&fat, kind, cluster)
                .ok_or_else(|| unsafe_fs(format!("FAT entry {cluster} is out of range")))?;
            if value == 0 {
                free_clusters.push(cluster);
            } else if is_bad(kind, value) {
                bad_clusters.insert(cluster);
            } else if !is_eoc(kind, value)
                && (value < 2 || value >= geometry.count_of_clusters.saturating_add(2))
            {
                writable_issues.push(format!(
                    "cluster {cluster} points outside the data area to {value}"
                ));
            }
            if value != 0 && !is_bad(kind, value) && !ownership.contains_key(&cluster) {
                writable_issues.push(format!("allocated cluster {cluster} is orphaned"));
            }
        }

        let ranges = allocation_ranges(&geometry, kind, &fat, &directory_clusters, &bad_clusters);
        Ok(Self {
            kind,
            geometry,
            boot,
            fat,
            files,
            directory_clusters,
            free_clusters,
            bad_clusters,
            ranges,
            writable_issues,
        })
    }

    pub(crate) fn writable(&self) -> bool {
        matches!(self.kind, ClassicKind::Fat16 | ClassicKind::Fat32)
            && self.writable_issues.is_empty()
    }

    pub(crate) fn equivalent_to(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.boot == other.boot
            && self.fat == other.fat
            && self.files == other.files
            && self.directory_clusters == other.directory_clusters
    }

    pub(crate) fn cluster_size(&self) -> u64 {
        u64::from(self.geometry.cluster_size)
    }

    pub(crate) fn cluster_offset(&self, cluster: u32) -> Result<u64, ServiceError> {
        self.geometry
            .cluster_offset(cluster)
            .ok_or_else(|| unsafe_fs(format!("cluster {cluster} has no byte offset")))
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_directory(
    reader: &FatFs<File>,
    geometry: &Geometry,
    kind: ClassicKind,
    fat: &[u8],
    id: FileId,
    path: &Path,
    seen_directories: &mut HashSet<u32>,
    directory_clusters: &mut HashSet<u32>,
    ownership: &mut HashMap<u32, String>,
    files: &mut Vec<RawFile>,
    issues: &mut Vec<String>,
) -> Result<(), ServiceError> {
    for node in reader.read_dir(id).map_err(fat_error)? {
        if node.is_deleted || node.is_volume_label || node.name == "." || node.name == ".." {
            continue;
        }
        let child_path = path.join(&node.name);
        let entry_offset = directory_entry_offset(geometry, kind, fat, node.id)?;
        if node.is_dir {
            if node.first_cluster < 2 || !seen_directories.insert(node.first_cluster) {
                issues.push(format!(
                    "directory {} has an invalid or repeated first cluster {}",
                    child_path.display(),
                    node.first_cluster
                ));
                continue;
            }
            let clusters = chain(fat, kind, node.first_cluster, geometry.count_of_clusters)?;
            register_chain(
                &clusters,
                &format!("directory {}", child_path.display()),
                ownership,
                issues,
            );
            directory_clusters.extend(clusters);
            walk_directory(
                reader,
                geometry,
                kind,
                fat,
                node.id,
                &child_path,
                seen_directories,
                directory_clusters,
                ownership,
                files,
                issues,
            )?;
            continue;
        }
        let clusters = if node.first_cluster < 2 {
            Vec::new()
        } else {
            chain(fat, kind, node.first_cluster, geometry.count_of_clusters)?
        };
        let expected = node.size.div_ceil(u64::from(geometry.cluster_size));
        if clusters.len() as u64 != expected {
            issues.push(format!(
                "{} has {} clusters but its size requires {expected}",
                child_path.display(),
                clusters.len()
            ));
        }
        register_chain(
            &clusters,
            &format!("file {}", child_path.display()),
            ownership,
            issues,
        );
        files.push(RawFile {
            path: child_path,
            size: node.size,
            attributes: node.attributes,
            entry_offset,
            first_cluster: node.first_cluster,
            chain: clusters,
        });
    }
    Ok(())
}

fn register_chain(
    clusters: &[u32],
    owner: &str,
    ownership: &mut HashMap<u32, String>,
    issues: &mut Vec<String>,
) {
    for &cluster in clusters {
        if let Some(previous) = ownership.insert(cluster, owner.to_owned()) {
            issues.push(format!(
                "cluster {cluster} is cross-linked between {previous} and {owner}"
            ));
        }
    }
}

fn directory_entry_offset(
    geometry: &Geometry,
    kind: ClassicKind,
    fat: &[u8],
    id: FileId,
) -> Result<u64, ServiceError> {
    let FileId::Entry { dir_cluster, index } = id else {
        return Err(unsafe_fs("root is not a file directory entry"));
    };
    let byte = u64::from(index) * 32;
    if dir_cluster == FIXED_ROOT {
        return Ok(geometry.root_dir_start.saturating_add(byte));
    }
    let parent = chain(fat, kind, dir_cluster, geometry.count_of_clusters)?;
    let cluster_index = usize::try_from(byte / u64::from(geometry.cluster_size))
        .map_err(|_| unsafe_fs("directory entry index overflows"))?;
    let within = byte % u64::from(geometry.cluster_size);
    let cluster = *parent
        .get(cluster_index)
        .ok_or_else(|| unsafe_fs("directory entry lies beyond its parent chain"))?;
    Ok(geometry
        .cluster_offset(cluster)
        .ok_or_else(|| unsafe_fs("directory cluster has no byte offset"))?
        .saturating_add(within))
}

pub(crate) fn chain(
    fat: &[u8],
    kind: ClassicKind,
    start: u32,
    cluster_count: u32,
) -> Result<Vec<u32>, ServiceError> {
    if start < 2 {
        return Ok(Vec::new());
    }
    let end = cluster_count.saturating_add(2);
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut current = start;
    loop {
        if current < 2 || current >= end {
            return Err(unsafe_fs(format!("cluster chain points to {current}")));
        }
        if !seen.insert(current) {
            return Err(unsafe_fs(format!("cluster chain cycles at {current}")));
        }
        result.push(current);
        if result.len() > cluster_count as usize {
            return Err(unsafe_fs("cluster chain is longer than the volume"));
        }
        let next = fat_entry(fat, kind, current)
            .ok_or_else(|| unsafe_fs(format!("FAT entry {current} is out of range")))?;
        if is_eoc(kind, next) {
            break;
        }
        if next == 0 || is_bad(kind, next) {
            return Err(unsafe_fs(format!(
                "cluster chain terminates at invalid value {next:#x}"
            )));
        }
        current = next;
    }
    Ok(result)
}

pub(crate) fn fat_entry(fat: &[u8], kind: ClassicKind, cluster: u32) -> Option<u32> {
    match kind {
        ClassicKind::Fat12 => {
            let offset = usize::try_from(cluster).ok()?.checked_mul(3)? / 2;
            let pair = u16::from_le_bytes([*fat.get(offset)?, *fat.get(offset + 1)?]);
            Some(if cluster & 1 == 0 {
                u32::from(pair & 0x0fff)
            } else {
                u32::from(pair >> 4)
            })
        }
        ClassicKind::Fat16 => {
            let offset = usize::try_from(cluster).ok()?.checked_mul(2)?;
            Some(u32::from(u16::from_le_bytes([
                *fat.get(offset)?,
                *fat.get(offset + 1)?,
            ])))
        }
        ClassicKind::Fat32 => {
            let offset = usize::try_from(cluster).ok()?.checked_mul(4)?;
            Some(
                u32::from_le_bytes([
                    *fat.get(offset)?,
                    *fat.get(offset + 1)?,
                    *fat.get(offset + 2)?,
                    *fat.get(offset + 3)?,
                ]) & 0x0fff_ffff,
            )
        }
    }
}

pub(crate) fn eoc(kind: ClassicKind) -> u32 {
    match kind {
        ClassicKind::Fat12 => 0x0fff,
        ClassicKind::Fat16 => 0xffff,
        ClassicKind::Fat32 => 0x0fff_ffff,
    }
}

pub(crate) struct ClassicWriter {
    file: File,
    pub snapshot: ClassicSnapshot,
    chains: Vec<Vec<u32>>,
    free: BTreeSet<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CopyState {
    Pending,
    Completed,
}

impl ClassicWriter {
    pub(crate) fn open_exclusive(source: &str) -> Result<Self, ServiceError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_EXCL | libc::O_CLOEXEC)
            .open(source)?;
        let snapshot = ClassicSnapshot::read_handle(&file)?;
        let chains = snapshot
            .files
            .iter()
            .map(|file| file.chain.clone())
            .collect();
        let free = snapshot.free_clusters.iter().copied().collect();
        Ok(Self {
            file,
            snapshot,
            chains,
            free,
        })
    }

    pub(crate) fn mark_dirty(&mut self) -> Result<(), ServiceError> {
        let mut value = fat_entry(&self.snapshot.fat, self.snapshot.kind, 1)
            .ok_or_else(|| unsafe_fs("FAT status entry is missing"))?;
        match self.snapshot.kind {
            ClassicKind::Fat12 => return Err(unsafe_fs("FAT12 writes are unsupported")),
            ClassicKind::Fat16 => value &= !(1 << 15),
            ClassicKind::Fat32 => value &= !(1 << 27),
        }
        self.set_fat_entry(1, value, true)
    }

    pub(crate) fn finish_clean(&mut self) -> Result<(), ServiceError> {
        self.update_fsinfo()?;
        let mut value = fat_entry(&self.snapshot.fat, self.snapshot.kind, 1)
            .ok_or_else(|| unsafe_fs("FAT status entry is missing"))?;
        match self.snapshot.kind {
            ClassicKind::Fat12 => return Err(unsafe_fs("FAT12 writes are unsupported")),
            ClassicKind::Fat16 => value |= (1 << 15) | (1 << 14),
            ClassicKind::Fat32 => value |= (1 << 27) | (1 << 26),
        }
        self.set_fat_entry(1, value, true)
    }

    pub(crate) fn place_file(
        &mut self,
        file_index: usize,
        target: &[u32],
        mut checkpoint: impl FnMut() -> Result<(), ServiceError>,
        mut copy_state: impl FnMut(CopyState, u64, u64, u64),
    ) -> Result<u64, ServiceError> {
        if target.is_empty() || self.chains.get(file_index).is_none() {
            return Ok(0);
        }
        let target_set: HashSet<_> = target.iter().copied().collect();
        let mut moved = 0u64;
        loop {
            let owners = self.owners()?;
            let blocker = target
                .iter()
                .find_map(|cluster| owners.get(cluster).copied().map(|owner| (*cluster, owner)));
            let Some((cluster, (owner, position))) = blocker else {
                break;
            };
            checkpoint()?;
            let scratch = self
                .free
                .iter()
                .copied()
                .find(|cluster| !target_set.contains(cluster))
                .ok_or_else(|| unsafe_fs("no scratch cluster exists outside the compact target"))?;
            let source_offset = self.snapshot.cluster_offset(cluster)?;
            let target_offset = self.snapshot.cluster_offset(scratch)?;
            copy_state(
                CopyState::Pending,
                source_offset,
                target_offset,
                self.snapshot.cluster_size(),
            );
            self.relocate_cluster(owner, position, scratch)?;
            copy_state(
                CopyState::Completed,
                source_offset,
                target_offset,
                self.snapshot.cluster_size(),
            );
            moved = moved.saturating_add(self.snapshot.cluster_size());
        }

        checkpoint()?;
        let source = self.chains[file_index].clone();
        if source.len() != target.len() {
            return Err(unsafe_fs(
                "planned target length no longer matches the file chain",
            ));
        }
        for (&from, &to) in source.iter().zip(target) {
            checkpoint()?;
            let source_offset = self.snapshot.cluster_offset(from)?;
            let target_offset = self.snapshot.cluster_offset(to)?;
            copy_state(
                CopyState::Pending,
                source_offset,
                target_offset,
                self.snapshot.cluster_size(),
            );
            self.copy_and_verify(source_offset, target_offset)?;
            copy_state(
                CopyState::Completed,
                source_offset,
                target_offset,
                self.snapshot.cluster_size(),
            );
            moved = moved.saturating_add(self.snapshot.cluster_size());
        }

        for (index, &cluster) in target.iter().enumerate() {
            let next = target
                .get(index + 1)
                .copied()
                .unwrap_or_else(|| eoc(self.snapshot.kind));
            self.set_fat_entry(cluster, next, false)?;
            self.free.remove(&cluster);
        }
        self.file.sync_all()?;
        self.set_first_cluster(file_index, target[0])?;
        self.file.sync_all()?;
        for cluster in &source {
            self.set_fat_entry(*cluster, 0, false)?;
            self.free.insert(*cluster);
        }
        self.file.sync_all()?;
        self.chains[file_index] = target.to_vec();
        Ok(moved)
    }

    pub(crate) fn current_chain(&self, file_index: usize) -> Option<&[u32]> {
        self.chains.get(file_index).map(Vec::as_slice)
    }

    pub(crate) fn reparse(&self) -> Result<ClassicSnapshot, ServiceError> {
        ClassicSnapshot::read_handle(&self.file)
    }

    fn owners(&self) -> Result<HashMap<u32, (usize, usize)>, ServiceError> {
        let mut result = HashMap::new();
        for (file_index, chain) in self.chains.iter().enumerate() {
            for (position, &cluster) in chain.iter().enumerate() {
                if result.insert(cluster, (file_index, position)).is_some() {
                    return Err(unsafe_fs(format!(
                        "cluster {cluster} became cross-linked during execution"
                    )));
                }
            }
        }
        Ok(result)
    }

    fn relocate_cluster(
        &mut self,
        file_index: usize,
        position: usize,
        destination: u32,
    ) -> Result<(), ServiceError> {
        let source = *self
            .chains
            .get(file_index)
            .and_then(|chain| chain.get(position))
            .ok_or_else(|| unsafe_fs("blocker chain changed during compaction"))?;
        if !self.free.contains(&destination) {
            return Err(unsafe_fs("scratch destination is not free"));
        }
        let next = self.chains[file_index]
            .get(position + 1)
            .copied()
            .unwrap_or_else(|| eoc(self.snapshot.kind));
        self.copy_and_verify(
            self.snapshot.cluster_offset(source)?,
            self.snapshot.cluster_offset(destination)?,
        )?;
        self.set_fat_entry(destination, next, true)?;
        if position == 0 {
            self.set_first_cluster(file_index, destination)?;
        } else {
            let predecessor = self.chains[file_index][position - 1];
            self.set_fat_entry(predecessor, destination, true)?;
        }
        self.file.sync_all()?;
        self.set_fat_entry(source, 0, true)?;
        self.chains[file_index][position] = destination;
        self.free.remove(&destination);
        self.free.insert(source);
        Ok(())
    }

    fn copy_and_verify(&self, source: u64, destination: u64) -> Result<(), ServiceError> {
        const CHUNK: usize = 1024 * 1024;
        let mut remaining = self.snapshot.cluster_size();
        let mut source_at = source;
        let mut destination_at = destination;
        let mut read = vec![0; CHUNK.min(remaining as usize)];
        let mut verify = vec![0; read.len()];
        while remaining > 0 {
            let length = read.len().min(remaining as usize);
            read_all_at(&self.file, &mut read[..length], source_at)?;
            write_all_at(&self.file, &read[..length], destination_at)?;
            self.file.sync_data()?;
            read_all_at(&self.file, &mut verify[..length], destination_at)?;
            if read[..length] != verify[..length] {
                return Err(unsafe_fs(format!(
                    "data verification failed at byte offset {destination_at}"
                )));
            }
            source_at = source_at.saturating_add(length as u64);
            destination_at = destination_at.saturating_add(length as u64);
            remaining -= length as u64;
        }
        Ok(())
    }

    fn set_first_cluster(&self, file_index: usize, cluster: u32) -> Result<(), ServiceError> {
        let entry = self
            .snapshot
            .files
            .get(file_index)
            .ok_or_else(|| unsafe_fs("file entry disappeared"))?;
        let mut bytes = [0u8; 32];
        read_all_at(&self.file, &mut bytes, entry.entry_offset)?;
        if self.snapshot.kind == ClassicKind::Fat32 {
            bytes[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        }
        bytes[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        write_all_at(&self.file, &bytes, entry.entry_offset)?;
        Ok(())
    }

    fn set_fat_entry(&mut self, cluster: u32, value: u32, sync: bool) -> Result<(), ServiceError> {
        let offset = fat_entry_offset(self.snapshot.kind, cluster)?;
        let fat_bytes = u64::from(self.snapshot.geometry.fat_size_sectors)
            .saturating_mul(u64::from(self.snapshot.geometry.bytes_per_sector));
        let mut encoded = match self.snapshot.kind {
            ClassicKind::Fat12 => return Err(unsafe_fs("FAT12 writes are unsupported")),
            ClassicKind::Fat16 => (value as u16).to_le_bytes().to_vec(),
            ClassicKind::Fat32 => {
                let current = u32::from_le_bytes(
                    self.snapshot.fat[offset..offset + 4]
                        .try_into()
                        .map_err(|_| unsafe_fs("FAT32 entry is truncated"))?,
                );
                ((current & 0xf000_0000) | (value & 0x0fff_ffff))
                    .to_le_bytes()
                    .to_vec()
            }
        };
        for copy in (0..self.snapshot.geometry.num_fats).rev() {
            let physical = self
                .snapshot
                .geometry
                .fat_start
                .saturating_add(u64::from(copy).saturating_mul(fat_bytes))
                .saturating_add(offset as u64);
            write_all_at(&self.file, &encoded, physical)?;
        }
        self.snapshot.fat[offset..offset + encoded.len()].swap_with_slice(&mut encoded);
        if sync {
            self.file.sync_all()?;
        }
        Ok(())
    }

    fn update_fsinfo(&self) -> Result<(), ServiceError> {
        if self.snapshot.kind != ClassicKind::Fat32 {
            self.file.sync_all()?;
            return Ok(());
        }
        let sector = u64::from(u16::from_le_bytes([
            self.snapshot.boot[48],
            self.snapshot.boot[49],
        ]));
        if sector == 0 || sector == 0xffff {
            return Ok(());
        }
        let bytes_per_sector = u64::from(self.snapshot.geometry.bytes_per_sector);
        let free_count = u32::try_from(self.free.len()).unwrap_or(u32::MAX);
        let next = self.free.first().copied().unwrap_or(0xffff_ffff);
        for fsinfo_sector in [
            Some(sector),
            backup_fsinfo_sector(&self.snapshot.boot, sector),
        ]
        .into_iter()
        .flatten()
        {
            let offset = fsinfo_sector.saturating_mul(bytes_per_sector);
            let mut bytes = vec![0; self.snapshot.geometry.bytes_per_sector as usize];
            read_all_at(&self.file, &mut bytes, offset)?;
            if bytes.get(0..4) == Some(&0x4161_5252u32.to_le_bytes())
                && bytes.get(484..488) == Some(&0x6141_7272u32.to_le_bytes())
            {
                bytes[488..492].copy_from_slice(&free_count.to_le_bytes());
                bytes[492..496].copy_from_slice(&next.to_le_bytes());
                write_all_at(&self.file, &bytes, offset)?;
            }
        }
        self.file.sync_all()?;
        Ok(())
    }
}

fn backup_fsinfo_sector(boot: &[u8; 512], fsinfo: u64) -> Option<u64> {
    let backup = u64::from(u16::from_le_bytes([boot[50], boot[51]]));
    (backup != 0 && backup != 0xffff).then_some(backup.saturating_add(fsinfo))
}

fn fat_entry_offset(kind: ClassicKind, cluster: u32) -> Result<usize, ServiceError> {
    let cluster = usize::try_from(cluster).map_err(|_| unsafe_fs("cluster index overflows"))?;
    match kind {
        ClassicKind::Fat12 => Err(unsafe_fs("FAT12 writes are unsupported")),
        ClassicKind::Fat16 => cluster
            .checked_mul(2)
            .ok_or_else(|| unsafe_fs("FAT16 entry offset overflows")),
        ClassicKind::Fat32 => cluster
            .checked_mul(4)
            .ok_or_else(|| unsafe_fs("FAT32 entry offset overflows")),
    }
}

fn read_all_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = file.read_at(bytes, offset)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short device read",
            ));
        }
        offset = offset.saturating_add(count as u64);
        bytes = &mut bytes[count..];
    }
    Ok(())
}

fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = file.write_at(bytes, offset)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short device write",
            ));
        }
        offset = offset.saturating_add(count as u64);
        bytes = &bytes[count..];
    }
    Ok(())
}

fn is_eoc(kind: ClassicKind, value: u32) -> bool {
    match kind {
        ClassicKind::Fat12 => value >= 0x0ff8,
        ClassicKind::Fat16 => value >= 0xfff8,
        ClassicKind::Fat32 => value >= 0x0fff_fff8,
    }
}

fn is_bad(kind: ClassicKind, value: u32) -> bool {
    value
        == match kind {
            ClassicKind::Fat12 => 0x0ff7,
            ClassicKind::Fat16 => 0xfff7,
            ClassicKind::Fat32 => 0x0fff_fff7,
        }
}

fn allocation_ranges(
    geometry: &Geometry,
    kind: ClassicKind,
    fat: &[u8],
    directories: &HashSet<u32>,
    bad: &HashSet<u32>,
) -> Vec<FsMapRange> {
    let mut ranges = Vec::new();
    if geometry.fat_start > 0 {
        ranges.push(FsMapRange {
            physical: 0,
            length: geometry.fat_start,
            kind: FsMapKind::Metadata(MetadataKind::FilesystemHeaders),
        });
    }
    let fat_bytes =
        u64::from(geometry.fat_size_sectors).saturating_mul(u64::from(geometry.bytes_per_sector));
    for copy in 0..geometry.num_fats {
        ranges.push(FsMapRange {
            physical: geometry
                .fat_start
                .saturating_add(u64::from(copy).saturating_mul(fat_bytes)),
            length: fat_bytes,
            kind: FsMapKind::Metadata(MetadataKind::AllocationTables),
        });
    }
    if geometry.root_dir_bytes > 0 {
        ranges.push(FsMapRange {
            physical: geometry.root_dir_start,
            length: u64::from(geometry.root_dir_bytes),
            kind: FsMapKind::Metadata(MetadataKind::FileMetadata),
        });
    }
    let mut start = 2u32;
    let mut previous_kind = cluster_kind(kind, fat, directories, bad, start);
    for cluster in 3..geometry.count_of_clusters.saturating_add(2) {
        let next_kind = cluster_kind(kind, fat, directories, bad, cluster);
        if next_kind != previous_kind {
            push_cluster_range(&mut ranges, geometry, start, cluster, previous_kind);
            start = cluster;
            previous_kind = next_kind;
        }
    }
    push_cluster_range(
        &mut ranges,
        geometry,
        start,
        geometry.count_of_clusters.saturating_add(2),
        previous_kind,
    );
    ranges
}

fn cluster_kind(
    kind: ClassicKind,
    fat: &[u8],
    directories: &HashSet<u32>,
    bad: &HashSet<u32>,
    cluster: u32,
) -> FsMapKind {
    if directories.contains(&cluster) {
        FsMapKind::Metadata(MetadataKind::FileMetadata)
    } else if bad.contains(&cluster) {
        FsMapKind::Metadata(MetadataKind::Reserved)
    } else if fat_entry(fat, kind, cluster) == Some(0) {
        FsMapKind::Free
    } else {
        FsMapKind::Allocated
    }
}

fn push_cluster_range(
    ranges: &mut Vec<FsMapRange>,
    geometry: &Geometry,
    start: u32,
    end: u32,
    kind: FsMapKind,
) {
    if end <= start {
        return;
    }
    if let Some(physical) = geometry.cluster_offset(start) {
        ranges.push(FsMapRange {
            physical,
            length: u64::from(end - start).saturating_mul(u64::from(geometry.cluster_size)),
            kind,
        });
    }
}

fn read_exact_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(bytes)
}

fn unsafe_fs(message: impl Into<String>) -> ServiceError {
    ServiceError::UnsafeFilesystem(message.into())
}

fn fat_error(error: fat::FatError) -> ServiceError {
    unsafe_fs(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fat16_and_fat32_entries() {
        let mut fat16 = vec![0; 12];
        fat16[4..6].copy_from_slice(&7u16.to_le_bytes());
        assert_eq!(fat_entry(&fat16, ClassicKind::Fat16, 2), Some(7));

        let mut fat32 = vec![0; 20];
        fat32[8..12].copy_from_slice(&0xf123_4567u32.to_le_bytes());
        assert_eq!(fat_entry(&fat32, ClassicKind::Fat32, 2), Some(0x0123_4567));
    }

    #[test]
    fn detects_chain_cycles() {
        let mut fat = vec![0; 12];
        fat[4..6].copy_from_slice(&3u16.to_le_bytes());
        fat[6..8].copy_from_slice(&2u16.to_le_bytes());
        assert!(chain(&fat, ClassicKind::Fat16, 2, 4).is_err());
    }
}
