use std::{
    collections::HashSet,
    fs::{self, File, Metadata},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use defrag_domain::{
    AnalysisCompleteness, AnalysisPhase, AnalysisReport, DefragPolicy, ExecutionRequirements,
    FileReport, FragmentationMetrics, JobId, JobProgress, PlanCandidate, PlanSummary,
    RequiredMountState, ScanCoverage, SupportStatus, Volume,
};

use crate::{
    EventSink, FilesystemAnalysis, FilesystemBackend, JobControl, PreparedPlan, ServiceError,
    block_map::BinAccumulator,
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
        if FAT_FILESYSTEMS.contains(&volume.filesystem.as_str()) {
            SupportStatus::ReadOnly
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

        // Linux FAT and exFAT expose per-file block mappings, but not the
        // filesystem-wide GETFSMAP interface used by ext4. Keep all bytes
        // unknown until a file mapping positively identifies them.
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

        let mut stack = vec![volume.mount_point.clone()];
        let mut seen_inodes = HashSet::new();
        let mut files = Vec::new();
        let mut scanned_ranges = Vec::new();
        let mut coverage = ScanCoverage {
            // statvfs used bytes include filesystem metadata, so this is an
            // intentionally conservative denominator for file-data coverage.
            total_allocated_data_bytes: volume.used_bytes,
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
                    Ok(mount_id) if mount_id != volume.mount_id => continue,
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
        }))
    }
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
}

impl FilesystemAnalysis for FatAnalysis {
    fn report(&self) -> &AnalysisReport {
        &self.report
    }

    fn build_plan(&self, policy: &DefragPolicy) -> Result<Box<dyn PreparedPlan>, ServiceError> {
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
        Ok(Box::new(FatPlan {
            summary: PlanSummary {
                volume_id: self.report.volume.id,
                candidates,
                estimated_rewrite_bytes,
                excluded_files,
                warnings: vec![
                    "Preview only: this build contains no FAT or exFAT cluster-moving operation."
                        .to_owned(),
                    "A future writer must operate offline and revalidate the allocation chains."
                        .to_owned(),
                ],
                requirements: ExecutionRequirements {
                    mount_state: RequiredMountState::Unmounted,
                    requires_privilege: true,
                    available_in_this_build: false,
                },
            },
        }))
    }
}

struct FatPlan {
    summary: PlanSummary,
}

impl PreparedPlan for FatPlan {
    fn summary(&self) -> &PlanSummary {
        &self.summary
    }

    fn execution_requirements(&self) -> &ExecutionRequirements {
        &self.summary.requirements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(filesystem: &str) -> Volume {
        Volume {
            id: defrag_domain::VolumeId(1),
            mount_id: 1,
            parent_mount_id: 0,
            device_major: 8,
            device_minor: 1,
            mount_point: PathBuf::from("/media/test"),
            source: "/dev/sda1".to_owned(),
            filesystem: filesystem.to_owned(),
            read_only: false,
            capacity_bytes: 1024,
            used_bytes: 512,
            free_bytes: 512,
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
}
