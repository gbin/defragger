use std::{
    collections::HashSet,
    fs::{self, File, Metadata},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use defrag_domain::{
    AnalysisCompleteness, AnalysisPhase, AnalysisReport, CategoryMix, DefragPolicy,
    ExecutionRequirements, FileReport, FragmentationMetrics, JobId, JobProgress, MapBin,
    MetadataMix, PlanCandidate, PlanSummary, RequiredMountState, ScanCoverage, SupportStatus,
    Volume,
};

use crate::{
    EventSink, FilesystemAnalysis, FilesystemBackend, JobControl, PreparedPlan, ServiceError,
    linux::{
        self, EXT4_EXTENTS_FL, EXT4_INLINE_DATA_FL, FIEMAP_EXTENT_DATA_ENCRYPTED,
        FIEMAP_EXTENT_DATA_INLINE, FIEMAP_EXTENT_DATA_TAIL, FIEMAP_EXTENT_DELALLOC,
        FIEMAP_EXTENT_ENCODED, FIEMAP_EXTENT_NOT_ALIGNED, FIEMAP_EXTENT_UNKNOWN,
        FIEMAP_EXTENT_UNWRITTEN, FS_APPEND_FL, FS_IMMUTABLE_FL, FileExtent, FsMapKind, FsMapRange,
        MetadataKind,
    },
};

pub struct Ext4Backend;

impl FilesystemBackend for Ext4Backend {
    fn id(&self) -> &'static str {
        "ext4"
    }

    fn probe(&self, volume: &Volume) -> SupportStatus {
        if volume.filesystem == "ext4" {
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

        let root = File::open(&volume.mount_point)?;
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
        let mut stack = vec![volume.mount_point.clone()];
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
    path == volume.mount_point.join("lost+found") || swaps.contains(path)
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

struct Ext4Analysis {
    report: AnalysisReport,
}

impl FilesystemAnalysis for Ext4Analysis {
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
        let mut warnings = vec![
            "Preview only: this build contains no extent-moving operation.".to_owned(),
            "A future writer must revalidate every candidate immediately before moving extents."
                .to_owned(),
        ];
        if self.report.completeness == AnalysisCompleteness::Partial {
            warnings.push("The plan is based on a partial analysis.".to_owned());
        }
        Ok(Box::new(Ext4Plan {
            summary: PlanSummary {
                volume_id: self.report.volume.id,
                candidates,
                estimated_rewrite_bytes,
                excluded_files,
                warnings,
                requirements: ExecutionRequirements {
                    mount_state: RequiredMountState::MountedReadWrite,
                    requires_privilege: true,
                    available_in_this_build: false,
                },
            },
        }))
    }
}

struct Ext4Plan {
    summary: PlanSummary,
}

impl PreparedPlan for Ext4Plan {
    fn summary(&self) -> &PlanSummary {
        &self.summary
    }

    fn execution_requirements(&self) -> &ExecutionRequirements {
        &self.summary.requirements
    }
}

const CATEGORY_COUNT: usize = 13;
const FREE: usize = 0;
const CONTIGUOUS_DATA: usize = 1;
const FRAGMENTED_DATA: usize = 2;
const UNSCANNED_DATA: usize = 3;
const FILESYSTEM_HEADERS: usize = 4;
const JOURNAL: usize = 5;
const ALLOCATION_TABLES: usize = 6;
const FILE_METADATA: usize = 7;
const GROUP_DESCRIPTORS: usize = 8;
const BLOCK_BITMAPS: usize = 9;
const FILE_BITMAPS: usize = 10;
const RESERVED: usize = 11;
const OTHER_METADATA: usize = 12;

struct BinAccumulator {
    start: u64,
    end: u64,
    span: u64,
    raw: Vec<[u64; CATEGORY_COUNT]>,
    changed: HashSet<usize>,
}

impl BinAccumulator {
    fn new(ranges: &[FsMapRange], capacity: u64, requested_bins: usize) -> Self {
        let start = ranges.iter().map(|range| range.physical).min().unwrap_or(0);
        let end = ranges
            .iter()
            .map(|range| range.physical.saturating_add(range.length))
            .max()
            .unwrap_or(capacity)
            .max(start.saturating_add(capacity));
        let total = end.saturating_sub(start).max(1);
        let count = requested_bins.min(total as usize).max(1);
        let span = total.div_ceil(count as u64).max(1);
        let mut accumulator = Self {
            start,
            end,
            span,
            raw: vec![[0; CATEGORY_COUNT]; count],
            changed: HashSet::new(),
        };
        // Absence of a GETFSMAP record is not evidence of free space. Begin
        // with every physical byte unknown, then replace only ranges that the
        // kernel positively identifies. FIEMAP progressively replaces more
        // unknown bytes while walking files.
        for index in 0..count {
            let offset = start + index as u64 * span;
            accumulator.raw[index][UNSCANNED_DATA] = end.saturating_sub(offset).min(span);
        }
        for range in ranges {
            let category = match range.kind {
                FsMapKind::Free => FREE,
                FsMapKind::Allocated => UNSCANNED_DATA,
                FsMapKind::Metadata(kind) => metadata_category(kind),
            };
            accumulator.assign(range.physical, range.length, category);
        }
        accumulator
    }

    fn assign(&mut self, physical: u64, length: u64, category: usize) {
        self.for_overlaps(physical, length, |raw, overlap| {
            let replaced = raw[UNSCANNED_DATA].min(overlap);
            raw[UNSCANNED_DATA] -= replaced;
            raw[category] = raw[category].saturating_add(replaced);
        });
    }

    fn mark_scanned(&mut self, physical: u64, length: u64, fragmented: bool) {
        let target = if fragmented {
            FRAGMENTED_DATA
        } else {
            CONTIGUOUS_DATA
        };
        let mut changed = Vec::new();
        self.for_overlaps_indexed(physical, length, |index, raw, overlap| {
            let moved = raw[UNSCANNED_DATA].min(overlap);
            raw[UNSCANNED_DATA] -= moved;
            raw[target] = raw[target].saturating_add(moved);
            if moved > 0 {
                changed.push(index);
            }
        });
        self.changed.extend(changed);
    }

    fn for_overlaps(
        &mut self,
        physical: u64,
        length: u64,
        mut action: impl FnMut(&mut [u64; CATEGORY_COUNT], u64),
    ) {
        self.for_overlaps_indexed(physical, length, |_, raw, overlap| action(raw, overlap));
    }

    fn for_overlaps_indexed(
        &mut self,
        physical: u64,
        length: u64,
        mut action: impl FnMut(usize, &mut [u64; CATEGORY_COUNT], u64),
    ) {
        let range_end = physical.saturating_add(length);
        if length == 0 || range_end <= self.start {
            return;
        }
        let first = physical.saturating_sub(self.start) / self.span;
        let last = range_end.saturating_sub(1).saturating_sub(self.start) / self.span;
        for index in first..=last.min(self.raw.len().saturating_sub(1) as u64) {
            let bin_start = self.start.saturating_add(index.saturating_mul(self.span));
            let bin_end = bin_start.saturating_add(self.span);
            let overlap = range_end
                .min(bin_end)
                .saturating_sub(physical.max(bin_start));
            if overlap > 0 {
                action(index as usize, &mut self.raw[index as usize], overlap);
            }
        }
    }

    fn snapshot(&self) -> Vec<MapBin> {
        (0..self.raw.len()).map(|index| self.bin(index)).collect()
    }

    fn take_changes(&mut self) -> Vec<MapBin> {
        let mut indices: Vec<_> = self.changed.drain().collect();
        indices.sort_unstable();
        indices.into_iter().map(|index| self.bin(index)).collect()
    }

    fn bin(&self, index: usize) -> MapBin {
        let raw = self.raw[index];
        let offset = self.start + index as u64 * self.span;
        let length = self.end.saturating_sub(offset).min(self.span).max(1);
        let part = |value: u64| (value as u128 * 10_000 / length as u128).min(10_000) as u16;
        MapBin {
            offset_bytes: offset,
            length_bytes: length,
            mix: CategoryMix {
                free: part(raw[FREE]),
                contiguous_data: part(raw[CONTIGUOUS_DATA]),
                fragmented_data: part(raw[FRAGMENTED_DATA]),
                unscanned_data: part(raw[UNSCANNED_DATA]),
                metadata: MetadataMix {
                    filesystem_headers: part(raw[FILESYSTEM_HEADERS]),
                    journal: part(raw[JOURNAL]),
                    allocation_tables: part(raw[ALLOCATION_TABLES]),
                    file_metadata: part(raw[FILE_METADATA]),
                    group_descriptors: part(raw[GROUP_DESCRIPTORS]),
                    block_bitmaps: part(raw[BLOCK_BITMAPS]),
                    file_bitmaps: part(raw[FILE_BITMAPS]),
                    reserved: part(raw[RESERVED]),
                    other: part(raw[OTHER_METADATA]),
                },
            },
        }
    }

    fn finish(self) -> Vec<MapBin> {
        self.snapshot()
    }
}

fn metadata_category(kind: MetadataKind) -> usize {
    match kind {
        MetadataKind::FilesystemHeaders => FILESYSTEM_HEADERS,
        MetadataKind::Journal => JOURNAL,
        MetadataKind::FileMetadata => FILE_METADATA,
        MetadataKind::GroupDescriptors => GROUP_DESCRIPTORS,
        MetadataKind::BlockBitmaps => BLOCK_BITMAPS,
        MetadataKind::FileBitmaps => FILE_BITMAPS,
        MetadataKind::Reserved => RESERVED,
        MetadataKind::Other => OTHER_METADATA,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn scanned_ranges_replace_unscanned_map_data() {
        let ranges = [FsMapRange {
            physical: 0,
            length: 100,
            kind: FsMapKind::Allocated,
        }];
        let mut bins = BinAccumulator::new(&ranges, 100, 1);
        bins.mark_scanned(0, 40, false);
        let bin = bins.finish().remove(0);
        assert_eq!(bin.mix.contiguous_data, 4000);
        assert_eq!(bin.mix.unscanned_data, 6000);
    }

    #[test]
    fn fragmented_data_is_separate_for_priority_coloring() {
        let ranges = [FsMapRange {
            physical: 0,
            length: 100,
            kind: FsMapKind::Allocated,
        }];
        let mut bins = BinAccumulator::new(&ranges, 100, 1);
        bins.mark_scanned(0, 25, true);
        let bin = bins.finish().remove(0);
        assert_eq!(bin.mix.fragmented_data, 2500);
        assert_eq!(bin.mix.unscanned_data, 7500);
    }

    #[test]
    fn category_intensity_is_relative_to_the_whole_display_cell() {
        let ranges = [FsMapRange {
            physical: 0,
            length: 25,
            kind: FsMapKind::Free,
        }];
        let bin = BinAccumulator::new(&ranges, 100, 1).finish().remove(0);
        assert_eq!(bin.mix.free, 2500);
        assert_eq!(bin.mix.unscanned_data, 7500);
    }

    #[test]
    fn typed_metadata_reaches_the_domain_map() {
        let ranges = [FsMapRange {
            physical: 0,
            length: 40,
            kind: FsMapKind::Metadata(MetadataKind::Journal),
        }];
        let bin = BinAccumulator::new(&ranges, 100, 1).finish().remove(0);
        assert_eq!(bin.mix.metadata.journal, 4000);
        assert_eq!(bin.mix.unscanned_data, 6000);
    }
}
