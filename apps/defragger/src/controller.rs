#[cxx_qt::bridge(namespace = "defragger")]
mod qobject {
    #[namespace = ""]
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    impl cxx_qt::Threading for Controller {}

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, volumes_json)]
        #[qproperty(QString, display_map_json)]
        #[qproperty(QString, report_json)]
        #[qproperty(QString, plan_json)]
        #[qproperty(QString, status)]
        #[qproperty(i32, map_revision)]
        #[qproperty(i32, display_map_generation)]
        #[qproperty(bool, busy)]
        #[qproperty(bool, paused)]
        #[qproperty(f64, files_scanned)]
        #[qproperty(f64, bytes_scanned)]
        type Controller = super::ControllerRust;

        #[qinvokable]
        fn refresh(self: Pin<&mut Controller>);
        #[qinvokable]
        fn analyze(self: Pin<&mut Controller>, volume_id: &QString);
        #[qinvokable]
        fn pause(self: Pin<&mut Controller>);
        #[qinvokable]
        fn resume(self: Pin<&mut Controller>);
        #[qinvokable]
        fn stop(self: Pin<&mut Controller>);
        #[qinvokable]
        fn build_plan(self: Pin<&mut Controller>);
        #[qinvokable]
        fn render_map(
            self: Pin<&mut Controller>,
            width: f64,
            height: f64,
            capacity_bytes: f64,
            use_analysis: bool,
            generation: i32,
        );
    }
}

use std::{
    pin::Pin,
    sync::mpsc::{self, Sender},
    time::Duration,
};

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use defrag_domain::{
    AnalysisId, CategoryMix, DefragPolicy, MapBin, MetadataMix, ServiceEvent, VolumeId,
};
use defrag_service::InProcessClient;

enum WorkerCommand {
    Pause,
    Resume,
    Cancel,
}

enum UiUpdate {
    Map {
        full: bool,
        bins: Vec<MapBin>,
    },
    Progress {
        files: u64,
        bytes: u64,
        detail: String,
    },
    Finished {
        analysis_id: AnalysisId,
        report_json: String,
    },
    Cancelled,
    Failed(String),
}

pub struct ControllerRust {
    volumes_json: QString,
    display_map_json: QString,
    report_json: QString,
    plan_json: QString,
    status: QString,
    map_revision: i32,
    display_map_generation: i32,
    busy: bool,
    paused: bool,
    files_scanned: f64,
    bytes_scanned: f64,
    client: InProcessClient,
    worker: Option<Sender<WorkerCommand>>,
    analysis_id: Option<AnalysisId>,
    map_bins: Vec<MapBin>,
}

impl Default for ControllerRust {
    fn default() -> Self {
        Self {
            volumes_json: QString::from("[]"),
            display_map_json: QString::from("[]"),
            report_json: QString::from("{}"),
            plan_json: QString::from("{}"),
            status: QString::default(),
            map_revision: 0,
            display_map_generation: 0,
            busy: false,
            paused: false,
            files_scanned: 0.0,
            bytes_scanned: 0.0,
            client: InProcessClient::new(),
            worker: None,
            analysis_id: None,
            map_bins: Vec::new(),
        }
    }
}

impl qobject::Controller {
    fn refresh(mut self: Pin<&mut Self>) {
        match self.client.list_volumes() {
            Ok(volumes) => match serde_json::to_string(&volumes) {
                Ok(json) => {
                    self.as_mut().set_volumes_json(QString::from(&json));
                    self.as_mut().set_status(QString::default());
                }
                Err(error) => self
                    .as_mut()
                    .set_status(QString::from(&format!("Could not encode volumes: {error}"))),
            },
            Err(error) => self
                .as_mut()
                .set_status(QString::from(&format!("Could not list volumes: {error}"))),
        }
    }

    fn analyze(mut self: Pin<&mut Self>, volume_id: &QString) {
        if self.busy {
            return;
        }
        let Ok(volume_id) = volume_id.to_string().parse::<u64>() else {
            self.as_mut()
                .set_status(QString::from("Select a valid volume first"));
            return;
        };
        let handle = match self.client.start_analysis(VolumeId(volume_id)) {
            Ok(handle) => handle,
            Err(error) => {
                self.as_mut()
                    .set_status(QString::from(&format!("Analysis failed to start: {error}")));
                return;
            }
        };

        let (command_sender, command_receiver) = mpsc::channel();
        self.as_mut().rust_mut().worker = Some(command_sender);
        self.as_mut().rust_mut().analysis_id = None;
        self.as_mut().rust_mut().map_bins.clear();
        let revision = self.map_revision.wrapping_add(1);
        self.as_mut().set_map_revision(revision);
        self.as_mut().set_report_json(QString::from("{}"));
        self.as_mut().set_plan_json(QString::from("{}"));
        self.as_mut().set_files_scanned(0.0);
        self.as_mut().set_bytes_scanned(0.0);
        self.as_mut().set_paused(false);
        self.as_mut().set_busy(true);
        self.as_mut()
            .set_status(QString::from("Reading the ext4 allocation map…"));

        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            loop {
                loop {
                    match command_receiver.try_recv() {
                        Ok(WorkerCommand::Pause) => handle.pause(),
                        Ok(WorkerCommand::Resume) => handle.resume(),
                        Ok(WorkerCommand::Cancel) => handle.cancel(),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            handle.cancel();
                            return;
                        }
                    }
                }
                let update = match handle.events().recv_timeout(Duration::from_millis(50)) {
                    Ok(ServiceEvent::MapUpdated {
                        full_snapshot,
                        bins,
                        ..
                    }) => Some(UiUpdate::Map {
                        full: full_snapshot,
                        bins,
                    }),
                    Ok(ServiceEvent::Progress(progress)) => Some(UiUpdate::Progress {
                        files: progress.files_scanned,
                        bytes: progress.bytes_scanned,
                        detail: progress
                            .current_path
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| format!("{:?}", progress.phase)),
                    }),
                    Ok(ServiceEvent::AnalysisFinished {
                        analysis_id,
                        report,
                        ..
                    }) => {
                        let report_json = serde_json::to_string(&report)
                            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
                        Some(UiUpdate::Finished {
                            analysis_id,
                            report_json,
                        })
                    }
                    Ok(ServiceEvent::JobCancelled { .. }) => Some(UiUpdate::Cancelled),
                    Ok(ServiceEvent::Failed { message, .. }) => Some(UiUpdate::Failed(message)),
                    Ok(_) => None,
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if let Some(update) = update {
                    let terminal = matches!(
                        update,
                        UiUpdate::Finished { .. } | UiUpdate::Cancelled | UiUpdate::Failed(_)
                    );
                    if qt_thread
                        .queue(move |controller| controller.apply_update(update))
                        .is_err()
                    {
                        handle.cancel();
                        break;
                    }
                    if terminal {
                        break;
                    }
                }
            }
        });
    }

    fn pause(mut self: Pin<&mut Self>) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(WorkerCommand::Pause);
            self.as_mut().set_paused(true);
            self.as_mut().set_status(QString::from("Analysis paused"));
        }
    }

    fn resume(mut self: Pin<&mut Self>) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(WorkerCommand::Resume);
            self.as_mut().set_paused(false);
            self.as_mut().set_status(QString::from("Analysis resumed"));
        }
    }

    fn stop(mut self: Pin<&mut Self>) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(WorkerCommand::Cancel);
            self.as_mut()
                .set_status(QString::from("Stopping analysis…"));
        }
    }

    fn build_plan(mut self: Pin<&mut Self>) {
        let Some(analysis_id) = self.analysis_id else {
            self.as_mut()
                .set_status(QString::from("Analyze the selected volume first"));
            return;
        };
        let policy = DefragPolicy {
            minimum_excess_runs: 1,
            minimum_file_bytes: 0,
        };
        match self.client.build_plan(analysis_id, &policy) {
            Ok((_, plan)) => match serde_json::to_string(&plan) {
                Ok(json) => {
                    self.as_mut().set_plan_json(QString::from(&json));
                    self.as_mut().set_status(QString::from(
                        "Defragmentation preview ready (execution is disabled in v0)",
                    ));
                }
                Err(error) => self
                    .as_mut()
                    .set_status(QString::from(&format!("Could not encode plan: {error}"))),
            },
            Err(error) => self
                .as_mut()
                .set_status(QString::from(&format!("Could not build plan: {error}"))),
        }
    }

    fn render_map(
        self: Pin<&mut Self>,
        width: f64,
        height: f64,
        capacity_bytes: f64,
        use_analysis: bool,
        generation: i32,
    ) {
        let source = if use_analysis {
            self.map_bins.clone()
        } else {
            Vec::new()
        };
        let width = dimension(width);
        let height = dimension(height);
        let capacity_bytes = finite_u64(capacity_bytes);
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let bins = aggregate_map(&source, capacity_bytes, width, height);
            let json = serde_json::to_string(&bins).unwrap_or_else(|_| "[]".to_owned());
            let _ = qt_thread.queue(move |mut controller| {
                controller
                    .as_mut()
                    .set_display_map_json(QString::from(&json));
                controller.as_mut().set_display_map_generation(generation);
            });
        });
    }

    fn apply_update(mut self: Pin<&mut Self>, update: UiUpdate) {
        match update {
            UiUpdate::Map { full, bins } => {
                if full {
                    self.as_mut().rust_mut().map_bins = bins;
                } else {
                    for bin in bins {
                        match self
                            .map_bins
                            .binary_search_by_key(&bin.offset_bytes, |item| item.offset_bytes)
                        {
                            Ok(index) => self.as_mut().rust_mut().map_bins[index] = bin,
                            Err(index) => self.as_mut().rust_mut().map_bins.insert(index, bin),
                        }
                    }
                }
                let revision = self.map_revision.wrapping_add(1);
                self.as_mut().set_map_revision(revision);
            }
            UiUpdate::Progress {
                files,
                bytes,
                detail,
            } => {
                self.as_mut().set_files_scanned(files as f64);
                self.as_mut().set_bytes_scanned(bytes as f64);
                self.as_mut().set_status(QString::from(&detail));
            }
            UiUpdate::Finished {
                analysis_id,
                report_json,
            } => {
                self.as_mut().rust_mut().analysis_id = Some(analysis_id);
                self.as_mut().rust_mut().worker = None;
                self.as_mut().set_report_json(QString::from(&report_json));
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut().set_status(QString::default());
            }
            UiUpdate::Cancelled => {
                self.as_mut().rust_mut().worker = None;
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut()
                    .set_status(QString::from("Analysis cancelled"));
            }
            UiUpdate::Failed(message) => {
                self.as_mut().rust_mut().worker = None;
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut()
                    .set_status(QString::from(&format!("Analysis failed: {message}")));
            }
        }
    }
}

fn dimension(value: f64) -> u32 {
    if value.is_finite() && value > 0.0 {
        value.min(u32::MAX as f64).round() as u32
    } else {
        0
    }
}

fn finite_u64(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value.min(u64::MAX as f64).round() as u64
    } else {
        0
    }
}

fn aggregate_map(source: &[MapBin], capacity: u64, width: u32, height: u32) -> Vec<MapBin> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    const PITCH: u32 = 11;
    const MAX_BINS: usize = 4096;
    let columns = ((width.saturating_add(2)) / PITCH).max(1);
    let rows = ((height.saturating_add(2)) / PITCH).max(1);
    let available = usize::try_from(columns.saturating_mul(rows)).unwrap_or(usize::MAX);

    if source.is_empty() {
        if capacity == 0 {
            return Vec::new();
        }
        let capacity_limit = usize::try_from(capacity).unwrap_or(usize::MAX);
        let count = available.min(MAX_BINS).min(capacity_limit).max(1);
        return (0..count)
            .map(|index| {
                let start = partition_point(index, count, capacity);
                let end = partition_point(index + 1, count, capacity);
                MapBin {
                    offset_bytes: start,
                    length_bytes: end.saturating_sub(start).max(1),
                    mix: CategoryMix {
                        unscanned_data: 10_000,
                        ..CategoryMix::default()
                    },
                }
            })
            .collect();
    }

    let count = source.len().min(available);
    if count >= source.len() {
        return source.to_vec();
    }

    let mut result = Vec::with_capacity(count);
    for target in 0..count {
        let first = target * source.len() / count;
        let end = ((target + 1) * source.len() / count).max(first + 1);
        let mut length = 0u64;
        let mut categories = [0u128; 4];
        let mut metadata = [0u128; 9];

        for bin in &source[first..end] {
            let span = u128::from(bin.length_bytes);
            length = length.saturating_add(bin.length_bytes);
            categories[0] += u128::from(bin.mix.free) * span;
            categories[1] += u128::from(bin.mix.contiguous_data) * span;
            categories[2] += u128::from(bin.mix.fragmented_data) * span;
            categories[3] += u128::from(bin.mix.unscanned_data) * span;
            for (total, value) in metadata.iter_mut().zip(metadata_values(bin.mix.metadata)) {
                *total += u128::from(value) * span;
            }
        }

        let scale = |value| weighted_basis_points(value, length);
        let metadata = metadata.map(scale);
        result.push(MapBin {
            offset_bytes: source[first].offset_bytes,
            length_bytes: length,
            mix: CategoryMix {
                free: scale(categories[0]),
                contiguous_data: scale(categories[1]),
                fragmented_data: scale(categories[2]),
                unscanned_data: scale(categories[3]),
                metadata: MetadataMix {
                    filesystem_headers: metadata[0],
                    journal: metadata[1],
                    allocation_tables: metadata[2],
                    file_metadata: metadata[3],
                    group_descriptors: metadata[4],
                    block_bitmaps: metadata[5],
                    file_bitmaps: metadata[6],
                    reserved: metadata[7],
                    other: metadata[8],
                },
            },
        });
    }
    result
}

fn partition_point(index: usize, count: usize, capacity: u64) -> u64 {
    ((index as u128 * u128::from(capacity)) / count as u128) as u64
}

fn weighted_basis_points(value: u128, length: u64) -> u16 {
    if length == 0 {
        return 0;
    }
    let length = u128::from(length);
    let rounded = (value + length / 2) / length;
    u16::try_from(rounded.min(u128::from(u16::MAX))).unwrap_or(u16::MAX)
}

fn metadata_values(metadata: MetadataMix) -> [u16; 9] {
    [
        metadata.filesystem_headers,
        metadata.journal,
        metadata.allocation_tables,
        metadata.file_metadata,
        metadata.group_descriptors,
        metadata.block_bitmaps,
        metadata.file_bitmaps,
        metadata.reserved,
        metadata.other,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_becomes_not_analyzed_placeholders() {
        let bins = aggregate_map(&[], 1_000_000, 110, 110);
        assert_eq!(bins.len(), 100);
        assert!(bins.iter().all(|bin| bin.mix.unscanned_data == 10_000));
        assert_eq!(
            bins.iter().map(|bin| bin.length_bytes).sum::<u64>(),
            1_000_000
        );
    }

    #[test]
    fn aggregation_is_length_weighted() {
        let source = vec![
            MapBin {
                offset_bytes: 0,
                length_bytes: 25,
                mix: CategoryMix {
                    free: 10_000,
                    ..CategoryMix::default()
                },
            },
            MapBin {
                offset_bytes: 25,
                length_bytes: 75,
                mix: CategoryMix {
                    contiguous_data: 10_000,
                    ..CategoryMix::default()
                },
            },
        ];
        let bins = aggregate_map(&source, 100, 1, 1);
        assert_eq!(bins.len(), 1);
        assert_eq!(bins[0].mix.free, 2_500);
        assert_eq!(bins[0].mix.contiguous_data, 7_500);
    }
}
