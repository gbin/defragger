use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File, Metadata},
    io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
    time::{Duration, Instant},
};

use defrag_domain::{
    AnalysisCompleteness, AnalysisPhase, AnalysisReport, DefragPhase, DefragPolicy, DefragProgress,
    ExecutionRequirements, FileReport, FragmentationMetrics, JobId, JobProgress, MountState,
    OptimizationMode, PhysicalRange, PlanCandidate, PlanCandidateRole, PlanSummary,
    RequiredMountState, ScanCoverage, SupportStatus, Volume,
};

use crate::{
    AnalysisAccess, EventSink, FilesystemAnalysis, FilesystemBackend, JobControl, PlanExecution,
    PreparedPlan, ServiceError,
    block_map::BinAccumulator,
    classic_fat::{ClassicKind, ClassicSnapshot, ClassicWriter, RawFile},
    linux::{
        self, FIEMAP_EXTENT_DATA_ENCRYPTED, FIEMAP_EXTENT_DATA_INLINE, FIEMAP_EXTENT_DATA_TAIL,
        FIEMAP_EXTENT_DELALLOC, FIEMAP_EXTENT_ENCODED, FIEMAP_EXTENT_NOT_ALIGNED,
        FIEMAP_EXTENT_UNKNOWN, FIEMAP_EXTENT_UNWRITTEN, FileExtent,
    },
};

const FAT_FILESYSTEMS: &[&str] = &["fat", "msdos", "vfat", "exfat"];

pub struct FatBackend;

impl FilesystemBackend for FatBackend {
    fn id(&self) -> &'static str {
        "fat"
    }

    fn probe(&self, volume: &Volume) -> SupportStatus {
        if volume.filesystem == "exfat" || volume.mount_state != MountState::Unmounted {
            if FAT_FILESYSTEMS.contains(&volume.filesystem.as_str()) {
                SupportStatus::ReadOnly
            } else {
                SupportStatus::Unsupported {
                    reason: format!("{} analysis is not implemented", volume.filesystem),
                }
            }
        } else if FAT_FILESYSTEMS.contains(&volume.filesystem.as_str()) {
            SupportStatus::Defragmentable
        } else {
            SupportStatus::Unsupported {
                reason: format!("{} analysis is not implemented", volume.filesystem),
            }
        }
    }

    fn analysis_access(&self, volume: &Volume) -> AnalysisAccess {
        if volume.mount_state == MountState::Unmounted && volume.filesystem != "exfat" {
            AnalysisAccess::RawDevice
        } else {
            AnalysisAccess::MountedReadOnly
        }
    }

    fn analyze(
        &self,
        volume: &Volume,
        job_id: JobId,
        control: &dyn JobControl,
        events: &dyn EventSink,
    ) -> Result<Box<dyn FilesystemAnalysis>, ServiceError> {
        if volume.mount_state == MountState::Unmounted && volume.filesystem != "exfat" {
            return analyze_raw(volume, job_id, control, events);
        }
        analyze_mounted(volume, job_id, control, events)
    }
}

fn analyze_mounted(
    volume: &Volume,
    job_id: JobId,
    control: &dyn JobControl,
    events: &dyn EventSink,
) -> Result<Box<dyn FilesystemAnalysis>, ServiceError> {
    control.checkpoint()?;
    events.progress(progress(
        job_id,
        AnalysisPhase::ReadingAllocationMap,
        0,
        0,
        None,
    ));

    // Linux FAT and exFAT expose per-file block mappings, but not the
    // filesystem-wide GETFSMAP interface used by ext4. Keep all bytes
    // unknown until a file mapping positively identifies them.
    let mount_point = volume.mount_point.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "FAT volume is not mounted for analysis",
        )
    })?;
    let mut bins = BinAccumulator::new(&[], volume.capacity_bytes, 4096);
    events.map_updated(true, bins.snapshot());
    let mut warnings = vec![
            "File mappings are queried without forcing writeback; the snapshot may change while files are active."
                .to_owned(),
            format!(
                "{} does not expose a filesystem-wide allocation map through Linux; free space, allocation tables, directories, and other metadata remain unlocated.",
                family_name(&volume.filesystem)
            ),
            "Linux FAT and exFAT normally require CAP_SYS_RAWIO for per-file FIBMAP queries. Files that cannot be mapped are skipped."
                .to_owned(),
        ];

    let mut stack = vec![mount_point.clone()];
    let mut seen_inodes = HashSet::new();
    let mut files = Vec::new();
    let mut scanned_ranges = Vec::new();
    let mut coverage = ScanCoverage {
        // statvfs used bytes include filesystem metadata, so this is an
        // intentionally conservative denominator for file-data coverage.
        total_allocated_data_bytes: volume.used_bytes.unwrap_or(0),
        ..ScanCoverage::default()
    };
    let mut metrics = FragmentationMetrics::default();
    let mut last_ui_update = Instant::now();
    let mut mapping_error = None;

    while let Some(directory) = stack.pop() {
        control.checkpoint()?;
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                continue;
            }
        };
        coverage.directories_scanned = coverage.directories_scanned.saturating_add(1);

        for entry in entries {
            control.checkpoint()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            match linux::mount_id(&path) {
                Ok(mount_id) if Some(mount_id) != volume.mount_id => continue,
                Err(_) => {
                    coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                    continue;
                }
                _ => {}
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() || !seen_inodes.insert((metadata.dev(), metadata.ino())) {
                continue;
            }

            let file = match File::open(&path) {
                Ok(file) => file,
                Err(_) => {
                    coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                    continue;
                }
            };
            let extents = match linux::fat_file_extents(&file, metadata.len()) {
                Ok(extents) => extents,
                Err(error) => {
                    mapping_error.get_or_insert_with(|| error.to_string());
                    coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                    continue;
                }
            };
            let report = inspect_file(path.clone(), &metadata, &extents);
            coverage.files_scanned = coverage.files_scanned.saturating_add(1);
            coverage.scanned_allocated_bytes = coverage
                .scanned_allocated_bytes
                .saturating_add(report.allocated_bytes);
            metrics.total_physical_runs = metrics
                .total_physical_runs
                .saturating_add(report.physical_runs as u64);
            metrics.total_excess_runs = metrics
                .total_excess_runs
                .saturating_add(report.excess_runs as u64);
            if report.excess_runs > 0 {
                metrics.fragmented_files = metrics.fragmented_files.saturating_add(1);
                metrics.fragmented_allocated_bytes = metrics
                    .fragmented_allocated_bytes
                    .saturating_add(report.allocated_bytes);
            }
            let fragmented = report.excess_runs > 0;
            scanned_ranges.extend(
                extents
                    .iter()
                    .filter_map(physical_range)
                    .map(|(physical, length)| (physical, length, fragmented)),
            );
            files.push(report);

            if coverage.files_scanned % 128 == 0
                || last_ui_update.elapsed() >= Duration::from_millis(100)
            {
                publish_ranges(&mut bins, &mut scanned_ranges, events);
                events.progress(progress(
                    job_id,
                    AnalysisPhase::WalkingFiles,
                    coverage.files_scanned,
                    coverage.scanned_allocated_bytes,
                    Some(path),
                ));
                last_ui_update = Instant::now();
            }
        }
    }

    events.progress(progress(
        job_id,
        AnalysisPhase::BuildingReport,
        coverage.files_scanned,
        coverage.scanned_allocated_bytes,
        None,
    ));
    publish_ranges(&mut bins, &mut scanned_ranges, events);
    files.sort_by(|left, right| {
        right
            .excess_runs
            .cmp(&left.excess_runs)
            .then_with(|| right.allocated_bytes.cmp(&left.allocated_bytes))
    });

    coverage.estimated_basis_points = ratio_basis_points(
        coverage.scanned_allocated_bytes,
        coverage.total_allocated_data_bytes,
    );
    metrics.fragmented_basis_points = ratio_basis_points(
        metrics.fragmented_allocated_bytes,
        coverage.scanned_allocated_bytes,
    );
    metrics.average_run_bytes = coverage
        .scanned_allocated_bytes
        .checked_div(metrics.total_physical_runs)
        .unwrap_or(0);
    if coverage.skipped_entries > 0 {
        warnings.push(format!(
            "Partial file scan: {} entries could not be read.",
            coverage.skipped_entries
        ));
    }
    if let Some(error) = mapping_error {
        warnings.push(format!("At least one file mapping failed: {error}."));
    }

    Ok(Box::new(FatAnalysis {
        report: AnalysisReport {
            volume: volume.clone(),
            // The file walk may be exhaustive, but the physical map is
            // necessarily partial without a filesystem allocation map.
            completeness: AnalysisCompleteness::Partial,
            coverage,
            fragmentation: metrics,
            files,
            map: bins.finish(),
            warnings,
        },
        snapshot: None,
    }))
}

fn analyze_raw(
    volume: &Volume,
    job_id: JobId,
    control: &dyn JobControl,
    events: &dyn EventSink,
) -> Result<Box<dyn FilesystemAnalysis>, ServiceError> {
    control.checkpoint()?;
    events.progress(progress(
        job_id,
        AnalysisPhase::ReadingAllocationMap,
        0,
        0,
        None,
    ));
    let snapshot = ClassicSnapshot::read(&volume.source)?;
    let mut analyzed_volume = volume.clone();
    analyzed_volume.capacity_bytes = snapshot
        .geometry
        .total_sectors
        .saturating_mul(u64::from(snapshot.geometry.bytes_per_sector));
    let free_bytes = snapshot
        .free_clusters
        .len()
        .saturating_mul(snapshot.geometry.cluster_size as usize) as u64;
    analyzed_volume.free_bytes = Some(free_bytes);
    analyzed_volume.used_bytes = Some(analyzed_volume.capacity_bytes.saturating_sub(free_bytes));
    analyzed_volume.support = if snapshot.writable() {
        SupportStatus::Defragmentable
    } else {
        SupportStatus::ReadOnly
    };

    let mut bins = BinAccumulator::new(&snapshot.ranges, analyzed_volume.capacity_bytes, 4096);
    events.map_updated(true, bins.snapshot());
    let mut files = Vec::with_capacity(snapshot.files.len());
    let mut metrics = FragmentationMetrics::default();
    let mut allocated_bytes = 0u64;
    for (index, file) in snapshot.files.iter().enumerate() {
        control.checkpoint()?;
        let report = inspect_raw_file(file, &snapshot, snapshot.writable());
        allocated_bytes = allocated_bytes.saturating_add(report.allocated_bytes);
        metrics.total_physical_runs = metrics
            .total_physical_runs
            .saturating_add(report.physical_runs as u64);
        metrics.total_excess_runs = metrics
            .total_excess_runs
            .saturating_add(report.excess_runs as u64);
        if report.excess_runs > 0 {
            metrics.fragmented_files = metrics.fragmented_files.saturating_add(1);
            metrics.fragmented_allocated_bytes = metrics
                .fragmented_allocated_bytes
                .saturating_add(report.allocated_bytes);
        }
        for range in &report.physical_ranges {
            bins.mark_scanned(
                range.offset_bytes,
                range.length_bytes,
                report.excess_runs > 0,
            );
        }
        files.push(report);
        if index % 128 == 127 {
            let changes = bins.take_changes();
            if !changes.is_empty() {
                events.map_updated(false, changes);
            }
            events.progress(progress(
                job_id,
                AnalysisPhase::WalkingFiles,
                files.len() as u64,
                allocated_bytes,
                Some(file.path.clone()),
            ));
        }
    }
    let changes = bins.take_changes();
    if !changes.is_empty() {
        events.map_updated(false, changes);
    }
    events.progress(progress(
        job_id,
        AnalysisPhase::BuildingReport,
        files.len() as u64,
        allocated_bytes,
        None,
    ));
    files.sort_by(|left, right| {
        right
            .excess_runs
            .cmp(&left.excess_runs)
            .then_with(|| right.allocated_bytes.cmp(&left.allocated_bytes))
    });
    metrics.fragmented_basis_points =
        ratio_basis_points(metrics.fragmented_allocated_bytes, allocated_bytes);
    metrics.average_run_bytes = allocated_bytes
        .checked_div(metrics.total_physical_runs)
        .unwrap_or(0);
    let mut warnings = vec![
        "Classic FAT was parsed directly from an unmounted block device; no kernel block-map fallback was used."
            .to_owned(),
    ];
    if snapshot.kind == ClassicKind::Fat12 {
        warnings.push("FAT12 remains analysis-only.".to_owned());
    }
    warnings.extend(
        snapshot
            .writable_issues
            .iter()
            .map(|issue| format!("Write safety check: {issue}.")),
    );
    let completeness = if snapshot.writable_issues.is_empty() {
        AnalysisCompleteness::Complete
    } else {
        AnalysisCompleteness::Partial
    };
    let coverage = ScanCoverage {
        files_scanned: files.len() as u64,
        directories_scanned: snapshot.directory_clusters.len() as u64,
        skipped_entries: 0,
        scanned_allocated_bytes: allocated_bytes,
        total_allocated_data_bytes: allocated_bytes,
        estimated_basis_points: ratio_basis_points(allocated_bytes, allocated_bytes),
    };
    Ok(Box::new(FatAnalysis {
        report: AnalysisReport {
            volume: analyzed_volume,
            completeness,
            coverage,
            fragmentation: metrics,
            files,
            map: bins.finish(),
            warnings,
        },
        snapshot: Some(snapshot),
    }))
}

fn inspect_raw_file(file: &RawFile, snapshot: &ClassicSnapshot, writable: bool) -> FileReport {
    let physical_runs = count_cluster_runs(&file.chain);
    let allocated_bytes = file.chain.len() as u64 * snapshot.cluster_size();
    FileReport {
        path: file.path.clone(),
        logical_bytes: file.size,
        allocated_bytes,
        physical_runs,
        minimum_runs: u32::from(!file.chain.is_empty()),
        excess_runs: physical_runs.saturating_sub(u32::from(!file.chain.is_empty())),
        average_run_bytes: allocated_bytes
            .checked_div(u64::from(physical_runs))
            .unwrap_or(0),
        eligible_for_plan: writable && !file.chain.is_empty(),
        exclusion_reason: (!writable)
            .then(|| "the raw FAT snapshot did not pass write-safety validation".to_owned())
            .or_else(|| {
                file.chain
                    .is_empty()
                    .then(|| "no allocated clusters".to_owned())
            }),
        physical_ranges: cluster_ranges(&file.chain, snapshot),
    }
}

fn count_cluster_runs(chain: &[u32]) -> u32 {
    chain
        .iter()
        .enumerate()
        .fold(0u32, |runs, (index, cluster)| {
            runs.saturating_add(u32::from(index == 0 || *cluster != chain[index - 1] + 1))
        })
}

fn cluster_ranges(chain: &[u32], snapshot: &ClassicSnapshot) -> Vec<PhysicalRange> {
    let mut result: Vec<PhysicalRange> = Vec::new();
    for &cluster in chain {
        let Ok(offset) = snapshot.cluster_offset(cluster) else {
            continue;
        };
        match result.last_mut() {
            Some(last) if last.offset_bytes.saturating_add(last.length_bytes) == offset => {
                last.length_bytes = last.length_bytes.saturating_add(snapshot.cluster_size());
            }
            _ => result.push(PhysicalRange {
                offset_bytes: offset,
                length_bytes: snapshot.cluster_size(),
            }),
        }
    }
    result
}

fn family_name(filesystem: &str) -> &'static str {
    if filesystem == "exfat" {
        "exFAT"
    } else {
        "FAT"
    }
}

fn progress(
    job_id: JobId,
    phase: AnalysisPhase,
    files_scanned: u64,
    bytes_scanned: u64,
    current_path: Option<PathBuf>,
) -> JobProgress {
    JobProgress {
        job_id,
        phase,
        files_scanned,
        bytes_scanned,
        current_path,
    }
}

fn publish_ranges(
    bins: &mut BinAccumulator,
    ranges: &mut Vec<(u64, u64, bool)>,
    events: &dyn EventSink,
) {
    for (physical, length, fragmented) in ranges.drain(..) {
        bins.mark_scanned(physical, length, fragmented);
    }
    let changes = bins.take_changes();
    if !changes.is_empty() {
        events.map_updated(false, changes);
    }
}

fn inspect_file(path: PathBuf, metadata: &Metadata, extents: &[FileExtent]) -> FileReport {
    let allocated_bytes = extents
        .iter()
        .fold(0u64, |sum, extent| sum.saturating_add(extent.length));
    let physical_runs = count_runs(extents);
    let minimum_runs = u32::from(allocated_bytes > 0);
    let excess_runs = physical_runs.saturating_sub(minimum_runs);
    let average_run_bytes = if physical_runs == 0 {
        0
    } else {
        allocated_bytes / physical_runs as u64
    };
    let bad_extent_flags = extents.iter().fold(0, |flags, extent| {
        flags
            | (extent.flags
                & (FIEMAP_EXTENT_UNKNOWN
                    | FIEMAP_EXTENT_DELALLOC
                    | FIEMAP_EXTENT_ENCODED
                    | FIEMAP_EXTENT_DATA_ENCRYPTED
                    | FIEMAP_EXTENT_NOT_ALIGNED
                    | FIEMAP_EXTENT_DATA_INLINE
                    | FIEMAP_EXTENT_DATA_TAIL
                    | FIEMAP_EXTENT_UNWRITTEN))
    });
    let exclusion_reason = if bad_extent_flags != 0 {
        Some(format!("unsupported FIEMAP flags 0x{bad_extent_flags:x}"))
    } else if allocated_bytes == 0 {
        Some("no allocated extents".to_owned())
    } else {
        None
    };

    FileReport {
        path,
        logical_bytes: metadata.len(),
        allocated_bytes,
        physical_runs,
        minimum_runs,
        excess_runs,
        average_run_bytes,
        eligible_for_plan: exclusion_reason.is_none(),
        exclusion_reason,
        physical_ranges: extents
            .iter()
            .filter_map(physical_range)
            .map(|(offset_bytes, length_bytes)| PhysicalRange {
                offset_bytes,
                length_bytes,
            })
            .collect(),
    }
}

fn count_runs(extents: &[FileExtent]) -> u32 {
    let mut runs = 0u32;
    let mut previous: Option<&FileExtent> = None;
    for extent in extents {
        let contiguous = previous.is_some_and(|last| {
            last.logical.saturating_add(last.length) == extent.logical
                && last.physical.saturating_add(last.length) == extent.physical
        });
        if !contiguous {
            runs = runs.saturating_add(1);
        }
        previous = Some(extent);
    }
    runs
}

fn physical_range(extent: &FileExtent) -> Option<(u64, u64)> {
    let unusable = FIEMAP_EXTENT_UNKNOWN
        | FIEMAP_EXTENT_DELALLOC
        | FIEMAP_EXTENT_DATA_INLINE
        | FIEMAP_EXTENT_DATA_TAIL;
    (extent.length > 0 && extent.flags & unusable == 0).then_some((extent.physical, extent.length))
}

fn ratio_basis_points(numerator: u64, denominator: u64) -> Option<u16> {
    (denominator != 0)
        .then(|| (numerator as u128 * 10_000 / denominator as u128).min(10_000) as u16)
}

struct FatAnalysis {
    report: AnalysisReport,
    snapshot: Option<ClassicSnapshot>,
}

impl FilesystemAnalysis for FatAnalysis {
    fn report(&self) -> &AnalysisReport {
        &self.report
    }

    fn build_plan(&self, policy: &DefragPolicy) -> Result<Box<dyn PreparedPlan>, ServiceError> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(Box::new(FatPlan::unavailable(
                self.report.clone(),
                "FAT execution requires a complete raw-device analysis of an unmounted volume.",
            )));
        };
        if !snapshot.writable() {
            return Ok(Box::new(FatPlan::unavailable(
                self.report.clone(),
                "This FAT snapshot did not pass all write-safety checks.",
            )));
        }

        let moves = match policy.mode {
            OptimizationMode::Defragment => plan_defragment(snapshot, policy),
            OptimizationMode::Compact => plan_compact(snapshot),
        };
        let candidates = moves
            .iter()
            .map(|planned| {
                let file = &snapshot.files[planned.file_index];
                let file_bytes = file.chain.len() as u64 * snapshot.cluster_size();
                PlanCandidate {
                    path: file.path.clone(),
                    // Compact may first evacuate one occupant for every target
                    // cluster. Two full-file copies are therefore a safe upper
                    // bound for this move; defrag destinations start free.
                    rewrite_bytes: if policy.mode == OptimizationMode::Compact {
                        file_bytes.saturating_mul(2)
                    } else {
                        file_bytes
                    },
                    current_runs: count_cluster_runs(&file.chain),
                    target_runs: 1,
                    role: planned.role,
                }
            })
            .collect::<Vec<_>>();
        let estimated_rewrite_bytes = candidates.iter().fold(0u64, |sum, candidate| {
            sum.saturating_add(candidate.rewrite_bytes)
        });
        let excluded_files = self.report.files.len() as u64 - candidates.len() as u64;
        let mode_warning = match policy.mode {
            OptimizationMode::Defragment => {
                "Defrag uses only already-free contiguous destinations and skips files without one."
            }
            OptimizationMode::Compact => {
                "Compact packs movable files toward low cluster addresses and may relocate contiguous supporting files."
            }
        };
        Ok(Box::new(FatPlan {
            volume: self.report.volume.clone(),
            report: self.report.clone(),
            snapshot: Some(snapshot.clone()),
            moves,
            summary: PlanSummary {
                volume_id: self.report.volume.id,
                candidates,
                estimated_rewrite_bytes,
                excluded_files,
                warnings: vec![
                    mode_warning.to_owned(),
                    "FAT has no journal. Crash-safe ordering preserves referenced data, but interruption can leave orphan clusters for fsck to recover."
                        .to_owned(),
                    "The volume is revalidated and must remain unmounted for the whole operation."
                        .to_owned(),
                ],
                requirements: ExecutionRequirements {
                    mount_state: RequiredMountState::Unmounted,
                    requires_privilege: true,
                    available_in_this_build: true,
                },
            },
        }))
    }
}

#[derive(Clone, Debug)]
struct PlannedMove {
    file_index: usize,
    target: Vec<u32>,
    role: PlanCandidateRole,
}

fn free_runs(clusters: impl IntoIterator<Item = u32>) -> Vec<Vec<u32>> {
    let mut clusters = clusters.into_iter().collect::<Vec<_>>();
    clusters.sort_unstable();
    let mut runs: Vec<Vec<u32>> = Vec::new();
    for cluster in clusters {
        match runs.last_mut() {
            Some(run) if run.last().is_some_and(|last| cluster == *last + 1) => run.push(cluster),
            _ => runs.push(vec![cluster]),
        }
    }
    runs
}

fn plan_defragment(snapshot: &ClassicSnapshot, policy: &DefragPolicy) -> Vec<PlannedMove> {
    let minimum_excess = policy.minimum_excess_runs.max(1);
    let mut file_indexes = snapshot
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| {
            count_cluster_runs(&file.chain).saturating_sub(1) >= minimum_excess
                && file.size >= policy.minimum_file_bytes
                && !file.chain.is_empty()
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    file_indexes.sort_by(|&left, &right| {
        let left_file = &snapshot.files[left];
        let right_file = &snapshot.files[right];
        count_cluster_runs(&right_file.chain)
            .cmp(&count_cluster_runs(&left_file.chain))
            .then_with(|| right_file.chain.len().cmp(&left_file.chain.len()))
            .then_with(|| left_file.path.cmp(&right_file.path))
    });
    let mut runs = free_runs(snapshot.free_clusters.iter().copied());
    let mut planned = Vec::new();
    for file_index in file_indexes {
        let length = snapshot.files[file_index].chain.len();
        let best = runs
            .iter()
            .enumerate()
            .filter(|(_, run)| run.len() >= length)
            .min_by_key(|(_, run)| (run.len(), run[0]))
            .map(|(index, _)| index);
        let Some(best) = best else { continue };
        let target = runs[best].drain(..length).collect();
        if runs[best].is_empty() {
            runs.remove(best);
        }
        planned.push(PlannedMove {
            file_index,
            target,
            role: PlanCandidateRole::FragmentationTarget,
        });
    }
    planned
}

fn plan_compact(snapshot: &ClassicSnapshot) -> Vec<PlannedMove> {
    let free_count = snapshot.free_clusters.len();
    let mut pinned = snapshot.directory_clusters.clone();
    pinned.extend(snapshot.bad_clusters.iter().copied());
    let mut movable = BTreeSet::new();
    for (index, file) in snapshot.files.iter().enumerate() {
        if file.chain.is_empty() || file.chain.len() > free_count {
            pinned.extend(file.chain.iter().copied());
        } else {
            movable.insert(index);
        }
    }

    loop {
        let cluster_end = snapshot.geometry.count_of_clusters.saturating_add(2);
        let segments = free_runs((2..cluster_end).filter(|cluster| !pinned.contains(cluster)));
        let mut remaining = segments;
        let mut indexes = movable.iter().copied().collect::<Vec<_>>();
        indexes.sort_by(|&left, &right| {
            snapshot.files[right]
                .chain
                .len()
                .cmp(&snapshot.files[left].chain.len())
                .then_with(|| snapshot.files[left].path.cmp(&snapshot.files[right].path))
        });
        let mut assignments = Vec::new();
        let mut failed = None;
        for file_index in indexes {
            let length = snapshot.files[file_index].chain.len();
            let destination = remaining
                .iter()
                .enumerate()
                .find(|(_, run)| run.len() >= length)
                .map(|(index, _)| index);
            let Some(destination) = destination else {
                failed = Some(file_index);
                break;
            };
            let target = remaining[destination].drain(..length).collect::<Vec<_>>();
            if remaining[destination].is_empty() {
                remaining.remove(destination);
            }
            assignments.push((file_index, target));
        }
        if let Some(file_index) = failed {
            movable.remove(&file_index);
            pinned.extend(snapshot.files[file_index].chain.iter().copied());
            continue;
        }
        assignments.sort_by_key(|(_, target)| target[0]);
        return assignments
            .into_iter()
            .filter(|(index, target)| snapshot.files[*index].chain != *target)
            .map(|(file_index, target)| PlannedMove {
                role: if count_cluster_runs(&snapshot.files[file_index].chain) > 1 {
                    PlanCandidateRole::FragmentationTarget
                } else {
                    PlanCandidateRole::CompactionSupport
                },
                file_index,
                target,
            })
            .collect();
    }
}

struct FatPlan {
    volume: Volume,
    report: AnalysisReport,
    snapshot: Option<ClassicSnapshot>,
    moves: Vec<PlannedMove>,
    summary: PlanSummary,
}

impl FatPlan {
    fn unavailable(report: AnalysisReport, warning: &str) -> Self {
        Self {
            volume: report.volume.clone(),
            snapshot: None,
            moves: Vec::new(),
            summary: PlanSummary {
                volume_id: report.volume.id,
                candidates: Vec::new(),
                estimated_rewrite_bytes: 0,
                excluded_files: report.files.len() as u64,
                warnings: vec![warning.to_owned()],
                requirements: ExecutionRequirements {
                    mount_state: RequiredMountState::Unmounted,
                    requires_privilege: true,
                    available_in_this_build: false,
                },
            },
            report,
        }
    }
}

impl PreparedPlan for FatPlan {
    fn summary(&self) -> &PlanSummary {
        &self.summary
    }

    fn execution_requirements(&self) -> &ExecutionRequirements {
        &self.summary.requirements
    }

    fn execute(
        &self,
        job_id: JobId,
        control: &dyn JobControl,
        events: &dyn EventSink,
    ) -> Result<PlanExecution, ServiceError> {
        execute_fat_plan(self, job_id, control, events)
    }
}

fn execute_fat_plan(
    plan: &FatPlan,
    job_id: JobId,
    control: &dyn JobControl,
    events: &dyn EventSink,
) -> Result<PlanExecution, ServiceError> {
    let expected = plan
        .snapshot
        .as_ref()
        .ok_or(ServiceError::ExecutionUnavailable)?;
    control.checkpoint()?;
    ensure_unmounted_source(&plan.volume)?;
    events.defrag_progress(DefragProgress {
        job_id,
        phase: DefragPhase::Revalidating,
        files_completed: 0,
        files_total: plan.moves.len() as u64,
        bytes_moved: 0,
        bytes_total: plan.summary.estimated_rewrite_bytes,
        current_path: None,
    });
    let mut writer = ClassicWriter::open_exclusive(&plan.volume.source)?;
    if !writer.snapshot.equivalent_to(expected) || !writer.snapshot.writable() {
        return Err(ServiceError::UnsafeFilesystem(
            "the FAT allocation or directory snapshot changed after planning".to_owned(),
        ));
    }
    writer.mark_dirty()?;

    let mut report = plan.report.clone();
    let mut bytes_moved = 0u64;
    for (completed, planned) in plan.moves.iter().enumerate() {
        let path = expected.files[planned.file_index].path.clone();
        events.defrag_progress(DefragProgress {
            job_id,
            phase: DefragPhase::EvacuatingClusters,
            files_completed: completed as u64,
            files_total: plan.moves.len() as u64,
            bytes_moved,
            bytes_total: plan.summary.estimated_rewrite_bytes,
            current_path: Some(path.clone()),
        });
        let result = writer.place_file(
            planned.file_index,
            &planned.target,
            || control.checkpoint(),
            |reading, writing, length| {
                events.defrag_activity(
                    vec![PhysicalRange {
                        offset_bytes: reading,
                        length_bytes: length,
                    }],
                    vec![PhysicalRange {
                        offset_bytes: writing,
                        length_bytes: length,
                    }],
                );
            },
        );
        match result {
            Ok(moved) => bytes_moved = bytes_moved.saturating_add(moved),
            Err(ServiceError::Cancelled) => {
                writer.finish_clean()?;
                events.defrag_activity(Vec::new(), Vec::new());
                let snapshot = writer.reparse()?;
                let report = report_from_snapshot(&plan.volume, &snapshot);
                events.map_updated(true, report.map.clone());
                return Ok(PlanExecution {
                    report,
                    stopped: true,
                });
            }
            Err(error) => return Err(error),
        }
        let chain = writer
            .current_chain(planned.file_index)
            .ok_or_else(|| ServiceError::UnsafeFilesystem("moved file disappeared".to_owned()))?;
        let mut raw_file = expected.files[planned.file_index].clone();
        raw_file.chain = chain.to_vec();
        raw_file.first_cluster = chain.first().copied().unwrap_or(0);
        let updated = inspect_raw_file(&raw_file, expected, true);
        replace_report_file(&mut report, updated.clone());
        recompute_raw_fragmentation(&mut report);
        events.defrag_file_updated(updated, report.fragmentation.clone(), bytes_moved);
        events.defrag_progress(DefragProgress {
            job_id,
            phase: DefragPhase::CommittingMetadata,
            files_completed: completed as u64 + 1,
            files_total: plan.moves.len() as u64,
            bytes_moved,
            bytes_total: plan.summary.estimated_rewrite_bytes,
            current_path: Some(path),
        });
    }
    writer.finish_clean()?;
    events.defrag_activity(Vec::new(), Vec::new());
    let snapshot = writer.reparse()?;
    let report = report_from_snapshot(&plan.volume, &snapshot);
    events.map_updated(true, report.map.clone());
    Ok(PlanExecution {
        report,
        stopped: false,
    })
}

fn ensure_unmounted_source(volume: &Volume) -> Result<(), ServiceError> {
    let metadata = fs::metadata(&volume.source)?;
    if metadata.file_type().is_block_device()
        && (libc::major(metadata.rdev()) != volume.device_major
            || libc::minor(metadata.rdev()) != volume.device_minor)
    {
        return Err(ServiceError::UnsafeFilesystem(
            "the selected block device identity changed".to_owned(),
        ));
    }
    let needle = format!("{}:{}", volume.device_major, volume.device_minor);
    if metadata.file_type().is_block_device()
        && fs::read_to_string("/proc/self/mountinfo")?
            .lines()
            .any(|line| line.split_whitespace().nth(2) == Some(needle.as_str()))
    {
        return Err(ServiceError::UnsafeFilesystem(
            "the FAT volume is mounted; unmount it before execution".to_owned(),
        ));
    }
    Ok(())
}

fn report_from_snapshot(volume: &Volume, snapshot: &ClassicSnapshot) -> AnalysisReport {
    let mut volume = volume.clone();
    volume.capacity_bytes = snapshot
        .geometry
        .total_sectors
        .saturating_mul(u64::from(snapshot.geometry.bytes_per_sector));
    let free_bytes = snapshot.free_clusters.len() as u64 * snapshot.cluster_size();
    volume.free_bytes = Some(free_bytes);
    volume.used_bytes = Some(volume.capacity_bytes.saturating_sub(free_bytes));
    volume.support = if snapshot.writable() {
        SupportStatus::Defragmentable
    } else {
        SupportStatus::ReadOnly
    };
    let mut files = snapshot
        .files
        .iter()
        .map(|file| inspect_raw_file(file, snapshot, snapshot.writable()))
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        right
            .excess_runs
            .cmp(&left.excess_runs)
            .then_with(|| right.allocated_bytes.cmp(&left.allocated_bytes))
    });
    let allocated_bytes = files.iter().map(|file| file.allocated_bytes).sum();
    let mut report = AnalysisReport {
        volume,
        completeness: if snapshot.writable_issues.is_empty() {
            AnalysisCompleteness::Complete
        } else {
            AnalysisCompleteness::Partial
        },
        coverage: ScanCoverage {
            files_scanned: files.len() as u64,
            directories_scanned: snapshot.directory_clusters.len() as u64,
            skipped_entries: 0,
            scanned_allocated_bytes: allocated_bytes,
            total_allocated_data_bytes: allocated_bytes,
            estimated_basis_points: ratio_basis_points(allocated_bytes, allocated_bytes),
        },
        fragmentation: FragmentationMetrics::default(),
        files,
        map: Vec::new(),
        warnings: snapshot.writable_issues.clone(),
    };
    recompute_raw_fragmentation(&mut report);
    let mut bins = BinAccumulator::new(&snapshot.ranges, report.volume.capacity_bytes, 4096);
    for file in &report.files {
        for range in &file.physical_ranges {
            bins.mark_scanned(range.offset_bytes, range.length_bytes, file.excess_runs > 0);
        }
    }
    report.map = bins.finish();
    report
}

fn replace_report_file(report: &mut AnalysisReport, updated: FileReport) {
    if let Some(file) = report
        .files
        .iter_mut()
        .find(|file| file.path == updated.path)
    {
        *file = updated;
    }
}

fn recompute_raw_fragmentation(report: &mut AnalysisReport) {
    let mut metrics = FragmentationMetrics::default();
    for file in &report.files {
        metrics.total_physical_runs = metrics
            .total_physical_runs
            .saturating_add(file.physical_runs as u64);
        metrics.total_excess_runs = metrics
            .total_excess_runs
            .saturating_add(file.excess_runs as u64);
        if file.excess_runs > 0 {
            metrics.fragmented_files = metrics.fragmented_files.saturating_add(1);
            metrics.fragmented_allocated_bytes = metrics
                .fragmented_allocated_bytes
                .saturating_add(file.allocated_bytes);
        }
    }
    metrics.average_run_bytes = report
        .coverage
        .scanned_allocated_bytes
        .checked_div(metrics.total_physical_runs)
        .unwrap_or(0);
    metrics.fragmented_basis_points = ratio_basis_points(
        metrics.fragmented_allocated_bytes,
        report.coverage.scanned_allocated_bytes,
    );
    report.fragmentation = metrics;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Seek, SeekFrom, Write},
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};

    struct TestControl;

    impl JobControl for TestControl {
        fn checkpoint(&self) -> Result<(), ServiceError> {
            Ok(())
        }

        fn is_cancelled(&self) -> bool {
            false
        }
    }

    struct TestSink;

    impl EventSink for TestSink {
        fn progress(&self, _: JobProgress) {}
        fn map_updated(&self, _: bool, _: Vec<defrag_domain::MapBin>) {}
        fn defrag_progress(&self, _: DefragProgress) {}
        fn defrag_activity(&self, _: Vec<PhysicalRange>, _: Vec<PhysicalRange>) {}
        fn defrag_file_updated(&self, _: FileReport, _: FragmentationMetrics, _: u64) {}
    }

    struct CancellingControl {
        checkpoints: AtomicUsize,
        cancel_at: usize,
    }

    impl JobControl for CancellingControl {
        fn checkpoint(&self) -> Result<(), ServiceError> {
            let current = self.checkpoints.fetch_add(1, Ordering::Relaxed) + 1;
            if current >= self.cancel_at {
                Err(ServiceError::Cancelled)
            } else {
                Ok(())
            }
        }

        fn is_cancelled(&self) -> bool {
            self.checkpoints.load(Ordering::Relaxed) >= self.cancel_at
        }
    }

    struct TestImage(PathBuf);

    impl Drop for TestImage {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn fragmented_image(fat_type: FatType) -> TestImage {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "defragger-fat-{}-{}.img",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut disk = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create FAT test image");
        let (length, cluster_size) = match fat_type {
            FatType::Fat16 => (16 * 1024 * 1024, 1024),
            FatType::Fat32 => (40 * 1024 * 1024, 512),
            FatType::Fat12 => unreachable!(),
        };
        disk.set_len(length).expect("size FAT test image");
        disk.seek(SeekFrom::Start(0)).unwrap();
        fatfs::format_volume(
            &mut disk,
            FormatVolumeOptions::new()
                .fat_type(fat_type)
                .bytes_per_cluster(cluster_size),
        )
        .expect("format FAT test image");
        disk.seek(SeekFrom::Start(0)).unwrap();
        let filesystem = FileSystem::new(disk, FsOptions::new()).expect("mount FAT test image");
        {
            let root = filesystem.root_dir();
            let mut target = root.create_file("fragmented payload.bin").unwrap();
            let mut spacer = root.create_file("spacer.bin").unwrap();
            let target_block = vec![0x5a; cluster_size as usize];
            let spacer_block = vec![0xa5; cluster_size as usize];
            for _ in 0..48 {
                target.write_all(&target_block).unwrap();
                target.flush().unwrap();
                spacer.write_all(&spacer_block).unwrap();
                spacer.flush().unwrap();
            }
            drop(target);
            drop(spacer);
            root.remove("spacer.bin").unwrap();
            root.create_file("VFAT long filename.txt")
                .unwrap()
                .write_all(b"long-name-data")
                .unwrap();
        }
        filesystem.unmount().expect("unmount FAT test image");
        TestImage(path)
    }

    fn raw_volume(path: &std::path::Path) -> Volume {
        Volume {
            id: defrag_domain::VolumeId(42),
            mount_id: None,
            parent_mount_id: None,
            device_major: 0,
            device_minor: 0,
            mount_point: None,
            source: path.display().to_string(),
            filesystem: "vfat".to_owned(),
            label: None,
            uuid: None,
            mount_state: MountState::Unmounted,
            read_only: false,
            capacity_bytes: 0,
            used_bytes: None,
            free_bytes: None,
            support: SupportStatus::Defragmentable,
        }
    }

    fn exercise_mode(fat_type: FatType, mode: OptimizationMode) {
        let image = fragmented_image(fat_type);
        let volume = raw_volume(&image.0);
        let analysis = FatBackend
            .analyze(&volume, JobId(1), &TestControl, &TestSink)
            .expect("analyze generated FAT image");
        assert_eq!(
            analysis.report().completeness,
            AnalysisCompleteness::Complete
        );
        assert!(
            analysis
                .report()
                .files
                .iter()
                .any(|file| file.path.ends_with("VFAT long filename.txt"))
        );
        let before = analysis.report().fragmentation.total_excess_runs;
        assert!(before > 0);
        let plan = analysis
            .build_plan(&DefragPolicy {
                mode,
                minimum_excess_runs: 1,
                minimum_file_bytes: 0,
            })
            .expect("plan FAT optimization");
        assert!(plan.summary().requirements.available_in_this_build);
        assert!(!plan.summary().candidates.is_empty());
        let result = plan
            .execute(JobId(2), &TestControl, &TestSink)
            .expect("execute FAT optimization");
        assert!(!result.stopped);
        assert!(result.report.fragmentation.total_excess_runs < before);

        let disk = File::open(&image.0).unwrap();
        let filesystem = FileSystem::new(disk, FsOptions::new()).unwrap();
        let mut payload = Vec::new();
        filesystem
            .root_dir()
            .open_file("fragmented payload.bin")
            .unwrap()
            .read_to_end(&mut payload)
            .unwrap();
        assert_eq!(
            payload.len(),
            48 * filesystem.stats().unwrap().cluster_size() as usize
        );
        assert!(payload.iter().all(|byte| *byte == 0x5a));
    }

    fn volume(filesystem: &str) -> Volume {
        Volume {
            id: defrag_domain::VolumeId(1),
            mount_id: Some(1),
            parent_mount_id: Some(0),
            device_major: 8,
            device_minor: 1,
            mount_point: Some(PathBuf::from("/media/test")),
            source: "/dev/sda1".to_owned(),
            filesystem: filesystem.to_owned(),
            label: None,
            uuid: None,
            mount_state: defrag_domain::MountState::MountedReadWrite,
            read_only: false,
            capacity_bytes: 1024,
            used_bytes: Some(512),
            free_bytes: Some(512),
            support: SupportStatus::ReadOnly,
        }
    }

    #[test]
    fn probes_fat_variants_and_exfat() {
        let backend = FatBackend;
        for filesystem in FAT_FILESYSTEMS {
            assert_eq!(backend.probe(&volume(filesystem)), SupportStatus::ReadOnly);
        }
        assert!(matches!(
            backend.probe(&volume("ext4")),
            SupportStatus::Unsupported { .. }
        ));
    }

    #[test]
    fn discontinuous_cluster_extents_are_fragmented() {
        let extents = [
            FileExtent {
                logical: 0,
                physical: 4096,
                length: 4096,
                flags: 0,
            },
            FileExtent {
                logical: 4096,
                physical: 12_288,
                length: 4096,
                flags: 0,
            },
        ];
        assert_eq!(count_runs(&extents), 2);
    }

    #[test]
    fn defrag_and_compact_generated_fat16_and_fat32_images() {
        for fat_type in [FatType::Fat16, FatType::Fat32] {
            exercise_mode(fat_type, OptimizationMode::Defragment);
            exercise_mode(fat_type, OptimizationMode::Compact);
        }
    }

    #[test]
    fn cancellation_during_cluster_copy_leaves_a_clean_parseable_volume() {
        let image = fragmented_image(FatType::Fat32);
        let volume = raw_volume(&image.0);
        let analysis = FatBackend
            .analyze(&volume, JobId(20), &TestControl, &TestSink)
            .unwrap();
        let plan = analysis
            .build_plan(&DefragPolicy {
                mode: OptimizationMode::Defragment,
                minimum_excess_runs: 1,
                minimum_file_bytes: 0,
            })
            .unwrap();
        let control = CancellingControl {
            checkpoints: AtomicUsize::new(0),
            cancel_at: 4,
        };
        let result = plan.execute(JobId(21), &control, &TestSink).unwrap();
        assert!(result.stopped);
        let reparsed = ClassicSnapshot::read(&image.0.display().to_string()).unwrap();
        assert!(
            reparsed.writable(),
            "issues: {:?}",
            reparsed.writable_issues
        );
    }

    #[test]
    fn compact_pins_a_file_larger_than_total_free_space() {
        let image = fragmented_image(FatType::Fat16);
        let mut snapshot = ClassicSnapshot::read(&image.0.display().to_string()).unwrap();
        let file_index = snapshot
            .files
            .iter()
            .position(|file| file.path.ends_with("fragmented payload.bin"))
            .unwrap();
        snapshot.free_clusters.truncate(1);
        assert!(snapshot.files[file_index].chain.len() > snapshot.free_clusters.len());
        assert!(
            plan_compact(&snapshot)
                .iter()
                .all(|planned| planned.file_index != file_index)
        );
    }

    #[test]
    #[ignore = "requires root and an unmounted FAT16/32 loop device in DEFRAGGER_TEST_DEVICE"]
    fn loop_device_fat_optimization_is_consistent() {
        let source = std::env::var("DEFRAGGER_TEST_DEVICE")
            .expect("set DEFRAGGER_TEST_DEVICE to an unmounted fixture loop device");
        let mode = match std::env::var("DEFRAGGER_TEST_MODE").as_deref() {
            Ok("compact") => OptimizationMode::Compact,
            Ok("defrag") => OptimizationMode::Defragment,
            _ => panic!("set DEFRAGGER_TEST_MODE to defrag or compact"),
        };
        let metadata = fs::metadata(&source).unwrap();
        assert!(metadata.file_type().is_block_device());
        let mut volume = raw_volume(std::path::Path::new(&source));
        volume.device_major = libc::major(metadata.rdev());
        volume.device_minor = libc::minor(metadata.rdev());
        let analysis = FatBackend
            .analyze(&volume, JobId(10), &TestControl, &TestSink)
            .unwrap();
        assert!(analysis.report().files.iter().any(|file| {
            file.path
                .ends_with("Directory created by Linux VFAT/Nested VFAT long filename.txt")
        }));
        let before = analysis.report().fragmentation.total_excess_runs;
        assert!(before > 0);
        let plan = analysis
            .build_plan(&DefragPolicy {
                mode,
                minimum_excess_runs: 1,
                minimum_file_bytes: 0,
            })
            .unwrap();
        assert!(!plan.summary().candidates.is_empty());
        let execution = plan.execute(JobId(11), &TestControl, &TestSink).unwrap();
        assert!(execution.report.fragmentation.total_excess_runs < before);

        let filesystem = FileSystem::new(File::open(&source).unwrap(), FsOptions::new()).unwrap();
        let mut payload = Vec::new();
        filesystem
            .root_dir()
            .open_file("fragmented payload.bin")
            .unwrap()
            .read_to_end(&mut payload)
            .unwrap();
        assert!(!payload.is_empty());
        assert!(payload.iter().all(|byte| *byte == 0x5a));
        let mut nested = Vec::new();
        filesystem
            .root_dir()
            .open_file("Directory created by Linux VFAT/Nested VFAT long filename.txt")
            .unwrap()
            .read_to_end(&mut nested)
            .unwrap();
        assert_eq!(nested, b"nested-long-name-data");
    }
}
