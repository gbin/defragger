use std::collections::HashSet;

use defrag_domain::{CategoryMix, MapBin, MetadataMix};

use crate::linux::{FsMapKind, FsMapRange, MetadataKind};

const CATEGORY_COUNT: usize = 14;
const FREE: usize = 0;
const CONTIGUOUS_DATA: usize = 1;
const FRAGMENTED_DATA: usize = 2;
const UNSCANNED_DATA: usize = 3;
const DEFRAG_STAGING: usize = 4;
const FILESYSTEM_HEADERS: usize = 5;
const JOURNAL: usize = 6;
const ALLOCATION_TABLES: usize = 7;
const FILE_METADATA: usize = 8;
const GROUP_DESCRIPTORS: usize = 9;
const BLOCK_BITMAPS: usize = 10;
const FILE_BITMAPS: usize = 11;
const RESERVED: usize = 12;
const OTHER_METADATA: usize = 13;

pub(crate) struct BinAccumulator {
    start: u64,
    end: u64,
    span: u64,
    raw: Vec<[u64; CATEGORY_COUNT]>,
    changed: HashSet<usize>,
}

impl BinAccumulator {
    pub(crate) fn new(ranges: &[FsMapRange], capacity: u64, requested_bins: usize) -> Self {
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
        // Missing allocation-map records are not evidence of free space. Start
        // unknown and replace bytes only when a kernel API identifies them.
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

    pub(crate) fn mark_scanned(&mut self, physical: u64, length: u64, fragmented: bool) {
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

    pub(crate) fn mark_staging(&mut self, physical: u64, length: u64) {
        let mut changed = Vec::new();
        self.for_overlaps_indexed(physical, length, |index, raw, overlap| {
            let mut remaining = overlap;
            let mut moved = 0;
            // A write destination is normally free, but compaction may first
            // evacuate an occupied target. Staging is the transient visual
            // state in either case; committed map updates replace it later.
            for category in [FREE, UNSCANNED_DATA, CONTIGUOUS_DATA, FRAGMENTED_DATA] {
                let amount = raw[category].min(remaining);
                raw[category] -= amount;
                remaining -= amount;
                moved += amount;
                if remaining == 0 {
                    break;
                }
            }
            raw[DEFRAG_STAGING] = raw[DEFRAG_STAGING].saturating_add(moved);
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

    pub(crate) fn snapshot(&self) -> Vec<MapBin> {
        (0..self.raw.len()).map(|index| self.bin(index)).collect()
    }

    pub(crate) fn take_changes(&mut self) -> Vec<MapBin> {
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
                defrag_staging: part(raw[DEFRAG_STAGING]),
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

    pub(crate) fn finish(self) -> Vec<MapBin> {
        self.snapshot()
    }
}

fn metadata_category(kind: MetadataKind) -> usize {
    match kind {
        MetadataKind::FilesystemHeaders => FILESYSTEM_HEADERS,
        MetadataKind::AllocationTables => ALLOCATION_TABLES,
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

    #[test]
    fn staging_replaces_free_destination_bytes() {
        let ranges = [FsMapRange {
            physical: 0,
            length: 100,
            kind: FsMapKind::Free,
        }];
        let mut bins = BinAccumulator::new(&ranges, 100, 1);
        bins.mark_staging(0, 25);
        let bin = bins.finish().remove(0);
        assert_eq!(bin.mix.free, 7500);
        assert_eq!(bin.mix.defrag_staging, 2500);
    }
}
