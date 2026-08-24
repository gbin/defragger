use std::{
    collections::HashSet,
    fs::{self, File, Metadata, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use defrag_domain::{
    AnalysisCompleteness, AnalysisPhase, AnalysisReport, DefragPhase, DefragPolicy, DefragProgress,
    ExecutionRequirements, FileReport, FragmentationMetrics, JobId, JobProgress, MountState,
    OptimizationMode, PhysicalRange, PlanCandidate, PlanCandidateRole, PlanSummary,
    RequiredMountState, ScanCoverage, SupportStatus, Volume,
};

use crate::{
    EventSink, FilesystemAnalysis, FilesystemBackend, JobControl, PlanExecution, PreparedPlan,
    ServiceError,
    block_map::BinAccumulator,
    linux::{
        self, EXT4_EXTENTS_FL, EXT4_INLINE_DATA_FL, FIEMAP_EXTENT_DATA_ENCRYPTED,
        FIEMAP_EXTENT_DATA_INLINE, FIEMAP_EXTENT_DATA_TAIL, FIEMAP_EXTENT_DELALLOC,
        FIEMAP_EXTENT_ENCODED, FIEMAP_EXTENT_NOT_ALIGNED, FIEMAP_EXTENT_UNKNOWN,
        FIEMAP_EXTENT_UNWRITTEN, FS_APPEND_FL, FS_IMMUTABLE_FL, FileExtent, FsMapKind,
    },
};

const MOVE_CHUNK_BYTES: u64 = 16 * 1024 * 1024;

pub struct Ext4Backend;

impl FilesystemBackend for Ext4Backend {
    fn id(&self) -> &'static str {
        "ext4"
    }

    fn probe(&self, volume: &Volume) -> SupportStatus {
        if volume.filesystem == "ext4" {
            SupportStatus::Defragmentable
        } else {
            SupportStatus::Unsupported {
                reason: format!("{} analysis is not implemented", volume.filesystem),
            }
        }
    }

    fn analyze(
        &self,
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

        let mount_point = volume.mount_point.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "ext4 volume is not mounted for analysis",
            )
        })?;
        let root = File::open(mount_point)?;
        let mut warnings = vec![
            "FIEMAP is queried without forcing writeback; the snapshot may change while files are active."
                .to_owned(),
        ];
        let fs_ranges = match linux::fsmap(&root) {
            Ok(ranges) => ranges,
            Err(error) => {
                warnings.push(format!(
                    "The kernel allocation map is unavailable ({error}); the drive map and coverage estimate are incomplete."
                ));
                Vec::new()
            }
        };
        let mut bins = BinAccumulator::new(&fs_ranges, volume.capacity_bytes, 4096);
        events.map_updated(true, bins.snapshot());
        let total_allocated_data_bytes = fs_ranges
            .iter()
            .filter(|range| range.kind == FsMapKind::Allocated)
            .fold(0u64, |sum, range| sum.saturating_add(range.length));

        let swaps = active_swap_paths();
        let mut stack = vec![mount_point.clone()];
        let mut seen_inodes = HashSet::new();
        let mut files = Vec::new();
        let mut scanned_ranges = Vec::new();
        let mut coverage = ScanCoverage {
            total_allocated_data_bytes,
            ..ScanCoverage::default()
        };
        let mut metrics = FragmentationMetrics::default();
        let mut last_ui_update = Instant::now();

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
                if is_excluded_path(volume, &path, &swaps) {
                    coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                    continue;
                }
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
                let extents = match linux::fiemap(&file) {
                    Ok(extents) => extents,
                    Err(_) => {
                        coverage.skipped_entries = coverage.skipped_entries.saturating_add(1);
                        continue;
                    }
                };
                let report = inspect_file(path.clone(), &metadata, &file, &extents);
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
                    for (physical, length, fragmented) in scanned_ranges.drain(..) {
                        bins.mark_scanned(physical, length, fragmented);
                    }
                    let changes = bins.take_changes();
                    if !changes.is_empty() {
                        events.map_updated(false, changes);
                    }
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
        for (physical, length, fragmented) in scanned_ranges {
            bins.mark_scanned(physical, length, fragmented);
        }
        let changes = bins.take_changes();
        if !changes.is_empty() {
            events.map_updated(false, changes);
        }
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
        let completeness = if coverage.skipped_entries == 0 && !fs_ranges.is_empty() {
            AnalysisCompleteness::Complete
        } else {
            warnings.push(format!(
                "Partial analysis: {} entries could not be read or were deliberately excluded.",
                coverage.skipped_entries
            ));
            AnalysisCompleteness::Partial
        };

        Ok(Box::new(Ext4Analysis {
            report: AnalysisReport {
                volume: volume.clone(),
                completeness,
                coverage,
                fragmentation: metrics,
                files,
                map: bins.finish(),
                warnings,
            },
        }))
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

fn is_excluded_path(volume: &Volume, path: &Path, swaps: &HashSet<PathBuf>) -> bool {
    volume
        .mount_point
        .as_ref()
        .is_some_and(|root| path == root.join("lost+found"))
        || swaps.contains(path)
}

fn active_swap_paths() -> HashSet<PathBuf> {
    fs::read_to_string("/proc/swaps")
        .ok()
        .into_iter()
        .flat_map(|contents| {
            contents
                .lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().next().map(PathBuf::from))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn inspect_file(
    path: PathBuf,
    metadata: &Metadata,
    file: &File,
    extents: &[FileExtent],
) -> FileReport {
    let allocated_bytes = extents
        .iter()
        .fold(0u64, |sum, extent| sum.saturating_add(extent.length));
    let physical_runs = count_runs(extents);
    let block_size = metadata.blksize().max(1);
    let maximum_extent_bytes = block_size.saturating_mul(32_768);
    let minimum_runs = if allocated_bytes == 0 {
        0
    } else {
        allocated_bytes
            .div_ceil(maximum_extent_bytes)
            .min(u32::MAX as u64) as u32
    };
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
    let exclusion_reason = match linux::file_flags(file) {
        Ok(flags) if flags & FS_IMMUTABLE_FL != 0 => Some("immutable file".to_owned()),
        Ok(flags) if flags & FS_APPEND_FL != 0 => Some("append-only file".to_owned()),
        Ok(flags) if flags & EXT4_INLINE_DATA_FL != 0 => Some("inline data".to_owned()),
        Ok(flags) if flags & EXT4_EXTENTS_FL == 0 => {
            Some("legacy indirect block mapping".to_owned())
        }
        Err(error) => Some(format!("could not read ext4 file flags: {error}")),
        _ if bad_extent_flags != 0 => {
            Some(format!("unsupported FIEMAP flags 0x{bad_extent_flags:x}"))
        }
        _ if allocated_bytes == 0 => Some("no allocated extents".to_owned()),
        _ if !logical_data_is_dense(extents, metadata.len()) => {
            Some("sparse files are not moved by the first writer".to_owned())
        }
        _ => None,
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

fn logical_data_is_dense(extents: &[FileExtent], logical_bytes: u64) -> bool {
    if logical_bytes == 0 {
        return false;
    }
    let mut end = 0u64;
    for extent in extents {
        if extent.logical > end {
            return false;
        }
        end = end.max(extent.logical.saturating_add(extent.length));
    }
    end >= logical_bytes
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

struct Ext4Analysis {
    report: AnalysisReport,
}

impl FilesystemAnalysis for Ext4Analysis {
    fn report(&self) -> &AnalysisReport {
        &self.report
    }

    fn build_plan(&self, policy: &DefragPolicy) -> Result<Box<dyn PreparedPlan>, ServiceError> {
        if policy.mode != OptimizationMode::Defragment {
            return Err(ServiceError::UnsupportedOptimizationMode {
                filesystem: "ext4".to_owned(),
                mode: policy.mode,
            });
        }
        let minimum_excess = policy.minimum_excess_runs.max(1);
        let mut candidates: Vec<_> = self
            .report
            .files
            .iter()
            .filter(|file| {
                file.eligible_for_plan
                    && file.excess_runs >= minimum_excess
                    && file.logical_bytes >= policy.minimum_file_bytes
            })
            .map(|file| PlanCandidate {
                path: file.path.clone(),
                rewrite_bytes: file.allocated_bytes,
                current_runs: file.physical_runs,
                target_runs: file.minimum_runs.max(1),
                role: PlanCandidateRole::FragmentationTarget,
            })
            .collect();
        candidates.sort_by(|left, right| {
            right
                .current_runs
                .saturating_sub(right.target_runs)
                .cmp(&left.current_runs.saturating_sub(left.target_runs))
        });
        let estimated_rewrite_bytes = candidates.iter().fold(0u64, |sum, candidate| {
            sum.saturating_add(candidate.rewrite_bytes)
        });
        let excluded_files = self.report.files.len() as u64 - candidates.len() as u64;
        let mut warnings = vec![
            "Every candidate is reopened and revalidated immediately before moving extents."
                .to_owned(),
        ];
        if self.report.completeness == AnalysisCompleteness::Partial {
            warnings.push("The plan is based on a partial analysis.".to_owned());
        }
        let root = self.report.volume.mount_point.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "analysis has no filesystem root")
        })?;
        let relative_paths = candidates
            .iter()
            .map(|candidate| {
                candidate
                    .path
                    .strip_prefix(&root)
                    .map(Path::to_path_buf)
                    .map_err(|_| {
                        ServiceError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "candidate escaped the analyzed filesystem",
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Box::new(Ext4Plan {
            volume: self.report.volume.clone(),
            report: self.report.clone(),
            relative_paths,
            summary: PlanSummary {
                volume_id: self.report.volume.id,
                candidates,
                estimated_rewrite_bytes,
                excluded_files,
                warnings,
                requirements: ExecutionRequirements {
                    mount_state: RequiredMountState::MountedReadWrite,
                    requires_privilege: true,
                    available_in_this_build: true,
                },
            },
        }))
    }
}

struct Ext4Plan {
    volume: Volume,
    report: AnalysisReport,
    relative_paths: Vec<PathBuf>,
    summary: PlanSummary,
}

impl PreparedPlan for Ext4Plan {
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
        execute_plan(self, job_id, control, events)
    }
}

fn execute_plan(
    plan: &Ext4Plan,
    job_id: JobId,
    control: &dyn JobControl,
    events: &dyn EventSink,
) -> Result<PlanExecution, ServiceError> {
    control.checkpoint()?;
    let mounted = crate::mounts::mount_for_job(&plan.volume, true)?;
    let root =
        mounted.volume.mount_point.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "ext4 writer has no mount point")
        })?;
    let old_root =
        plan.volume.mount_point.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "ext4 plan has no analyzed root")
        })?;
    let mut report = plan.report.clone();
    for file in &mut report.files {
        if let Ok(relative) = file.path.strip_prefix(old_root) {
            file.path = root.join(relative);
        }
    }
    report.volume = mounted.volume.clone();

    let files_total = plan.relative_paths.len() as u64;
    let bytes_total = plan.summary.estimated_rewrite_bytes;
    let mut files_completed = 0u64;
    let mut bytes_moved = 0u64;

    for relative in &plan.relative_paths {
        control.checkpoint()?;
        let path = root.join(relative);
        events.defrag_progress(DefragProgress {
            job_id,
            phase: DefragPhase::Revalidating,
            files_completed,
            files_total,
            bytes_moved,
            bytes_total,
            current_path: Some(path.clone()),
        });
        let target = OpenOptions::new().read(true).write(true).open(&path)?;
        let metadata = target.metadata()?;
        if !metadata.is_file() {
            files_completed = files_completed.saturating_add(1);
            continue;
        }
        let before_extents = linux::fiemap_sync(&target)?;
        let before = inspect_file(path.clone(), &metadata, &target, &before_extents);
        if !before.eligible_for_plan || before.excess_runs == 0 {
            files_completed = files_completed.saturating_add(1);
            continue;
        }
        let block_size = linux::filesystem_block_size(&target)?;
        let allocation_bytes = metadata
            .len()
            .div_ceil(block_size)
            .saturating_mul(block_size);
        let donor = create_donor(path.parent().unwrap_or(root))?;
        events.defrag_progress(DefragProgress {
            job_id,
            phase: DefragPhase::AllocatingDonor,
            files_completed,
            files_total,
            bytes_moved,
            bytes_total,
            current_path: Some(path.clone()),
        });
        allocate_file(&donor, allocation_bytes)?;
        donor.sync_data()?;
        let donor_extents = linux::fiemap_sync(&donor)?;
        if donor_extents.is_empty() || count_runs(&donor_extents) >= before.physical_runs {
            files_completed = files_completed.saturating_add(1);
            continue;
        }

        refresh_report_map(root, &mut report, &extent_ranges(&donor_extents), events)?;
        let total_blocks = allocation_bytes / block_size;
        let chunk_blocks = (MOVE_CHUNK_BYTES / block_size).max(1);
        let mut logical_block = 0u64;
        while logical_block < total_blocks {
            control.checkpoint()?;
            let requested_blocks = chunk_blocks.min(total_blocks - logical_block);
            let logical_bytes = logical_block.saturating_mul(block_size);
            let requested_bytes = requested_blocks.saturating_mul(block_size);
            let current_extents = linux::fiemap_sync(&target)?;
            let reading = physical_for_logical(&current_extents, logical_bytes, requested_bytes);
            let writing = physical_for_logical(&donor_extents, logical_bytes, requested_bytes);
            events.defrag_activity(reading, writing);
            events.defrag_progress(DefragProgress {
                job_id,
                phase: DefragPhase::MovingExtents,
                files_completed,
                files_total,
                bytes_moved,
                bytes_total,
                current_path: Some(path.clone()),
            });

            let moved_blocks =
                linux::move_extents(&target, &donor, logical_block, requested_blocks)?;
            if moved_blocks == 0 {
                return Err(ServiceError::Io(io::Error::other(
                    "EXT4_IOC_MOVE_EXT completed without moving any blocks",
                )));
            }
            target.sync_data()?;
            let moved_bytes = moved_blocks.saturating_mul(block_size);
            punch_hole(&donor, logical_bytes, moved_bytes)?;
            donor.sync_data()?;

            logical_block = logical_block.saturating_add(moved_blocks);
            bytes_moved = bytes_moved.saturating_add(moved_bytes);
            let fresh_extents = linux::fiemap_sync(&target)?;
            let fresh = inspect_file(path.clone(), &target.metadata()?, &target, &fresh_extents);
            replace_file_report(&mut report, fresh.clone());
            recompute_fragmentation(&mut report);
            events.defrag_progress(DefragProgress {
                job_id,
                phase: DefragPhase::RefreshingMap,
                files_completed,
                files_total,
                bytes_moved,
                bytes_total,
                current_path: Some(path.clone()),
            });
            let staging = extent_ranges(&linux::fiemap_sync(&donor)?);
            refresh_report_map(root, &mut report, &staging, events)?;
            events.defrag_file_updated(fresh, report.fragmentation.clone(), bytes_moved);
            events.defrag_activity(Vec::new(), Vec::new());
            if control.is_cancelled() {
                drop(donor);
                refresh_report_map(root, &mut report, &[], events)?;
                normalize_finished_report(plan, root, &mut report);
                return Ok(PlanExecution {
                    report,
                    stopped: true,
                });
            }
            control.checkpoint()?;
        }
        files_completed = files_completed.saturating_add(1);
    }

    events.defrag_activity(Vec::new(), Vec::new());
    refresh_report_map(root, &mut report, &[], events)?;
    normalize_finished_report(plan, root, &mut report);
    Ok(PlanExecution {
        report,
        stopped: false,
    })
}

fn normalize_finished_report(plan: &Ext4Plan, root: &Path, report: &mut AnalysisReport) {
    if plan.volume.mount_state == MountState::Unmounted {
        let mut volume = plan.volume.clone();
        volume.capacity_bytes = report.volume.capacity_bytes;
        volume.used_bytes = report.volume.used_bytes;
        volume.free_bytes = report.volume.free_bytes;
        report.volume = volume;
        for file in &mut report.files {
            if let Ok(relative) = file.path.strip_prefix(root) {
                file.path = Path::new("/").join(relative);
            }
        }
    }
}

static NEXT_DONOR: AtomicU64 = AtomicU64::new(1);

fn create_donor(directory: &Path) -> io::Result<File> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_TMPFILE)
        .mode(0o600)
        .open(directory)
    {
        Ok(file) => Ok(file),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EOPNOTSUPP) | Some(libc::EINVAL)
            ) =>
        {
            let serial = NEXT_DONOR.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".defragger-{}-{serial}", std::process::id()));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            fs::remove_file(path)?;
            Ok(file)
        }
        Err(error) => Err(error),
    }
}

fn allocate_file(file: &File, length: u64) -> io::Result<()> {
    let length = i64::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::FileTooLarge, "donor is too large"))?;
    // SAFETY: fd is valid and fallocate does not access userspace pointers.
    if unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, length) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    let offset = i64::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "hole offset is too large"))?;
    let length = i64::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "hole length is too large"))?;
    // SAFETY: fd is valid and fallocate does not access userspace pointers.
    if unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset,
            length,
        )
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn physical_for_logical(extents: &[FileExtent], logical: u64, length: u64) -> Vec<PhysicalRange> {
    let end = logical.saturating_add(length);
    extents
        .iter()
        .filter_map(|extent| {
            let overlap_start = logical.max(extent.logical);
            let overlap_end = end.min(extent.logical.saturating_add(extent.length));
            (overlap_end > overlap_start).then_some(PhysicalRange {
                offset_bytes: extent
                    .physical
                    .saturating_add(overlap_start.saturating_sub(extent.logical)),
                length_bytes: overlap_end.saturating_sub(overlap_start),
            })
        })
        .collect()
}

fn extent_ranges(extents: &[FileExtent]) -> Vec<PhysicalRange> {
    extents
        .iter()
        .filter_map(|extent| {
            (extent.length > 0).then_some(PhysicalRange {
                offset_bytes: extent.physical,
                length_bytes: extent.length,
            })
        })
        .collect()
}

fn replace_file_report(report: &mut AnalysisReport, updated: FileReport) {
    if let Some(existing) = report
        .files
        .iter_mut()
        .find(|file| file.path == updated.path)
    {
        *existing = updated;
    }
}

fn recompute_fragmentation(report: &mut AnalysisReport) {
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

fn refresh_report_map(
    root: &Path,
    report: &mut AnalysisReport,
    staging: &[PhysicalRange],
    events: &dyn EventSink,
) -> Result<(), ServiceError> {
    let root_file = File::open(root)?;
    let ranges = linux::fsmap(&root_file).map_err(|error| match &error {
        linux::IoctlError::Io { source, .. } if source.raw_os_error() == Some(libc::EBADMSG) => {
            ServiceError::UnsafeFilesystem(
                "the kernel rejected the ext4 allocation map because filesystem metadata is inconsistent; keep the volume unmounted and run e2fsck"
                    .to_owned(),
            )
        }
        _ => ServiceError::Kernel(error),
    })?;
    let mut bins = BinAccumulator::new(&ranges, report.volume.capacity_bytes, 4096);
    for file in &report.files {
        let fragmented = file.excess_runs > 0;
        for range in &file.physical_ranges {
            bins.mark_scanned(range.offset_bytes, range.length_bytes, fragmented);
        }
    }
    for range in staging {
        bins.mark_staging(range.offset_bytes, range.length_bytes);
    }
    report.map = bins.finish();
    events.map_updated(true, report.map.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn adjacent_physical_extents_form_one_run() {
        let extents = [
            FileExtent {
                logical: 0,
                physical: 100,
                length: 10,
                flags: 0,
            },
            FileExtent {
                logical: 10,
                physical: 110,
                length: 10,
                flags: 0,
            },
            FileExtent {
                logical: 20,
                physical: 200,
                length: 10,
                flags: 0,
            },
        ];
        assert_eq!(count_runs(&extents), 2);
    }

    #[test]
    fn fragmentation_ratio_uses_scanned_allocated_bytes() {
        assert_eq!(ratio_basis_points(25, 100), Some(2500));
        assert_eq!(ratio_basis_points(0, 0), None);
    }

    #[test]
    #[ignore = "requires root, CAP_SYS_ADMIN, and an unmounted loop device in DEFRAGGER_TEST_DEVICE"]
    fn committed_fixture_defragments_without_changing_file_bytes() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        let source = std::env::var("DEFRAGGER_TEST_DEVICE")
            .expect("set DEFRAGGER_TEST_DEVICE to the fixture loop device");
        let metadata = fs::metadata(&source).expect("fixture device must exist");
        assert!(metadata.file_type().is_block_device());
        let volume = Volume {
            id: defrag_domain::VolumeId(
                (u64::from(libc::major(metadata.rdev())) << 32)
                    | u64::from(libc::minor(metadata.rdev())),
            ),
            mount_id: None,
            parent_mount_id: None,
            device_major: libc::major(metadata.rdev()),
            device_minor: libc::minor(metadata.rdev()),
            mount_point: None,
            source,
            filesystem: "ext4".into(),
            label: None,
            uuid: Some("11111111-2222-3333-4444-555555555555".into()),
            mount_state: MountState::Unmounted,
            read_only: false,
            capacity_bytes: 32 * 1024 * 1024,
            used_bytes: None,
            free_bytes: None,
            support: SupportStatus::Defragmentable,
        };
        let backend = Ext4Backend;
        eprintln!("fixture: mounting {} read-only for analysis", volume.source);
        let analysis_mount = crate::mounts::mount_for_job(&volume, false)
            .unwrap_or_else(|error| panic!("fixture analysis mount failed: {error}"));
        let root = analysis_mount.volume.mount_point.as_ref().unwrap();
        let before: Vec<_> = (0..4)
            .map(|index| fs::read(root.join(format!("target-{index}.bin"))).unwrap())
            .collect();
        eprintln!("fixture: analyzing the four deliberately fragmented files");
        let analysis = backend
            .analyze(&analysis_mount.volume, JobId(1), &TestControl, &TestSink)
            .unwrap_or_else(|error| panic!("fixture analysis failed: {error}"));
        assert!(
            analysis
                .report()
                .files
                .iter()
                .all(|file| file.physical_runs == 32)
        );
        let plan = analysis.build_plan(&DefragPolicy::default()).unwrap();
        assert_eq!(plan.summary().candidates.len(), 4);
        eprintln!("fixture: confirmed 32 fragments per file; executing four-file plan");
        drop(analysis_mount);

        let execution = plan
            .execute(JobId(2), &TestControl, &TestSink)
            .unwrap_or_else(|error| panic!("fixture defragmentation failed: {error}"));
        assert!(!execution.stopped);
        assert!(
            execution
                .report
                .files
                .iter()
                .filter(|file| file
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("target-")))
                .all(|file| file.excess_runs == 0)
        );

        eprintln!("fixture: verifying zero extra fragments and byte-for-byte contents");
        let verification_mount = crate::mounts::mount_for_job(&volume, false)
            .unwrap_or_else(|error| panic!("fixture verification mount failed: {error}"));
        let root = verification_mount.volume.mount_point.as_ref().unwrap();
        for (index, expected) in before.iter().enumerate() {
            assert_eq!(
                &fs::read(root.join(format!("target-{index}.bin"))).unwrap(),
                expected
            );
        }
        eprintln!("fixture: content and fragmentation checks passed");
    }
}
