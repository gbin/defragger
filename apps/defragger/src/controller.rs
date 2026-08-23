#[cxx_qt::bridge(namespace = "defragger")]
mod qobject {
    #[namespace = ""]
    unsafe extern "C++" {
        include!("cxx-qt-lib/qbytearray.h");
        type QByteArray = cxx_qt_lib::QByteArray;
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    impl cxx_qt::Threading for Controller {}

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QByteArray, display_map_data)]
        #[qproperty(QString, status)]
        #[qproperty(QString, map_volume_id)]
        #[qproperty(QString, report_volume_id)]
        #[qproperty(i32, volume_count)]
        #[qproperty(i32, map_revision)]
        #[qproperty(i32, display_map_generation)]
        #[qproperty(i32, fragmented_basis_points)]
        #[qproperty(i32, coverage_basis_points)]
        #[qproperty(i32, file_row_count)]
        #[qproperty(i32, plan_candidate_count)]
        #[qproperty(i32, plan_revision)]
        #[qproperty(bool, busy)]
        #[qproperty(bool, paused)]
        #[qproperty(bool, has_report)]
        #[qproperty(f64, files_scanned)]
        #[qproperty(f64, bytes_scanned)]
        #[qproperty(f64, skipped_entries)]
        #[qproperty(f64, plan_estimated_rewrite_bytes)]
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
        fn volume_id(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn volume_mount_point(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn volume_source(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn volume_filesystem(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn volume_capacity_bytes(self: &Controller, index: i32) -> f64;
        #[qinvokable]
        fn volume_used_bytes(self: &Controller, index: i32) -> f64;
        #[qinvokable]
        fn volume_free_bytes(self: &Controller, index: i32) -> f64;
        #[qinvokable]
        fn volume_supported(self: &Controller, index: i32) -> bool;
        #[qinvokable]
        fn file_path(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn file_physical_runs(self: &Controller, index: i32) -> i32;
        #[qinvokable]
        fn file_excess_runs(self: &Controller, index: i32) -> i32;
        #[qinvokable]
        fn plan_candidate_path(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn plan_candidate_current_runs(self: &Controller, index: i32) -> i32;
        #[qinvokable]
        fn plan_candidate_target_runs(self: &Controller, index: i32) -> i32;
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
use cxx_qt_lib::{QByteArray, QString};
use defrag_domain::{
    AnalysisId, AnalysisReport, CategoryMix, DefragPolicy, FileReport, MapBin, MetadataMix,
    PlanCandidate, ServiceEvent, SupportStatus, Volume, VolumeId,
};
use defrag_service::InProcessClient;

const MAP_RECORD_BYTES: usize = 42;

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
        report: UiReport,
    },
    Cancelled,
    Failed(String),
}

struct UiReport {
    volume_id: VolumeId,
    fragmented_basis_points: i32,
    coverage_basis_points: i32,
    files_scanned: f64,
    bytes_scanned: f64,
    skipped_entries: f64,
    status: String,
    map_bins: Vec<MapBin>,
    file_rows: Vec<FileReport>,
}

pub struct ControllerRust {
    display_map_data: QByteArray,
    status: QString,
    map_volume_id: QString,
    report_volume_id: QString,
    volume_count: i32,
    map_revision: i32,
    display_map_generation: i32,
    fragmented_basis_points: i32,
    coverage_basis_points: i32,
    file_row_count: i32,
    plan_candidate_count: i32,
    plan_revision: i32,
    busy: bool,
    paused: bool,
    has_report: bool,
    files_scanned: f64,
    bytes_scanned: f64,
    skipped_entries: f64,
    plan_estimated_rewrite_bytes: f64,
    client: InProcessClient,
    worker: Option<Sender<WorkerCommand>>,
    analysis_id: Option<AnalysisId>,
    volumes: Vec<Volume>,
    map_bins: Vec<MapBin>,
    file_rows: Vec<FileReport>,
    plan_candidates: Vec<PlanCandidate>,
}

impl Default for ControllerRust {
    fn default() -> Self {
        Self {
            display_map_data: QByteArray::default(),
            status: QString::default(),
            map_volume_id: QString::default(),
            report_volume_id: QString::default(),
            volume_count: 0,
            map_revision: 0,
            display_map_generation: 0,
            fragmented_basis_points: -1,
            coverage_basis_points: -1,
            file_row_count: 0,
            plan_candidate_count: 0,
            plan_revision: 0,
            busy: false,
            paused: false,
            has_report: false,
            files_scanned: 0.0,
            bytes_scanned: 0.0,
            skipped_entries: 0.0,
            plan_estimated_rewrite_bytes: 0.0,
            client: InProcessClient::new(),
            worker: None,
            analysis_id: None,
            volumes: Vec::new(),
            map_bins: Vec::new(),
            file_rows: Vec::new(),
            plan_candidates: Vec::new(),
        }
    }
}

impl qobject::Controller {
    fn refresh(mut self: Pin<&mut Self>) {
        match self.client.list_volumes() {
            Ok(volumes) => {
                let count = count_i32(volumes.len());
                self.as_mut().set_volume_count(0);
                self.as_mut().rust_mut().volumes = volumes;
                self.as_mut().set_volume_count(count);
                self.as_mut().set_status(QString::default());
            }
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
        self.as_mut()
            .set_map_volume_id(QString::from(&volume_id.to_string()));
        self.as_mut().rust_mut().map_bins.clear();
        self.as_mut().rust_mut().file_rows.clear();
        self.as_mut().rust_mut().plan_candidates.clear();
        let revision = self.map_revision.wrapping_add(1);
        self.as_mut().set_map_revision(revision);
        self.as_mut().set_report_volume_id(QString::default());
        self.as_mut().set_fragmented_basis_points(-1);
        self.as_mut().set_coverage_basis_points(-1);
        self.as_mut().set_file_row_count(0);
        self.as_mut().set_plan_candidate_count(0);
        self.as_mut().set_plan_revision(0);
        self.as_mut().set_has_report(false);
        self.as_mut().set_files_scanned(0.0);
        self.as_mut().set_bytes_scanned(0.0);
        self.as_mut().set_skipped_entries(0.0);
        self.as_mut().set_plan_estimated_rewrite_bytes(0.0);
        self.as_mut().set_paused(false);
        self.as_mut().set_busy(true);
        self.as_mut()
            .set_status(QString::from("Reading the filesystem allocation map…"));

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
                    }) => Some(UiUpdate::Finished {
                        analysis_id,
                        report: prepare_ui_report(report),
                    }),
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
            Ok((_, plan)) => {
                let count = count_i32(plan.candidates.len());
                let estimated_rewrite_bytes = plan.estimated_rewrite_bytes as f64;
                self.as_mut().rust_mut().plan_candidates = plan.candidates;
                self.as_mut().set_plan_candidate_count(count);
                self.as_mut()
                    .set_plan_estimated_rewrite_bytes(estimated_rewrite_bytes);
                let revision = self.plan_revision.wrapping_add(1).max(1);
                self.as_mut().set_plan_revision(revision);
                self.as_mut().set_status(QString::from(
                    "Defragmentation preview ready (execution is disabled in v0)",
                ));
            }
            Err(error) => self
                .as_mut()
                .set_status(QString::from(&format!("Could not build plan: {error}"))),
        }
    }

    fn volume_id(&self, index: i32) -> QString {
        self.volume_row(index)
            .map_or_else(QString::default, |volume| {
                QString::from(&volume.id.0.to_string())
            })
    }

    fn volume_mount_point(&self, index: i32) -> QString {
        self.volume_row(index)
            .map_or_else(QString::default, |volume| {
                QString::from(&volume.mount_point.display().to_string())
            })
    }

    fn volume_source(&self, index: i32) -> QString {
        self.volume_row(index)
            .map_or_else(QString::default, |volume| QString::from(&volume.source))
    }

    fn volume_filesystem(&self, index: i32) -> QString {
        self.volume_row(index)
            .map_or_else(QString::default, |volume| QString::from(&volume.filesystem))
    }

    fn volume_capacity_bytes(&self, index: i32) -> f64 {
        self.volume_row(index)
            .map_or(0.0, |volume| volume.capacity_bytes as f64)
    }

    fn volume_used_bytes(&self, index: i32) -> f64 {
        self.volume_row(index)
            .map_or(0.0, |volume| volume.used_bytes as f64)
    }

    fn volume_free_bytes(&self, index: i32) -> f64 {
        self.volume_row(index)
            .map_or(0.0, |volume| volume.free_bytes as f64)
    }

    fn volume_supported(&self, index: i32) -> bool {
        self.volume_row(index)
            .is_some_and(|volume| matches!(volume.support, SupportStatus::ReadOnly))
    }

    fn file_path(&self, index: i32) -> QString {
        self.file_row(index).map_or_else(QString::default, |file| {
            QString::from(&file.path.display().to_string())
        })
    }

    fn file_physical_runs(&self, index: i32) -> i32 {
        self.file_row(index)
            .map_or(0, |file| count_i32(file.physical_runs as usize))
    }

    fn file_excess_runs(&self, index: i32) -> i32 {
        self.file_row(index)
            .map_or(0, |file| count_i32(file.excess_runs as usize))
    }

    fn plan_candidate_path(&self, index: i32) -> QString {
        self.plan_candidate(index)
            .map_or_else(QString::default, |candidate| {
                QString::from(&candidate.path.display().to_string())
            })
    }

    fn plan_candidate_current_runs(&self, index: i32) -> i32 {
        self.plan_candidate(index)
            .map_or(0, |candidate| count_i32(candidate.current_runs as usize))
    }

    fn plan_candidate_target_runs(&self, index: i32) -> i32 {
        self.plan_candidate(index)
            .map_or(0, |candidate| count_i32(candidate.target_runs as usize))
    }

    fn file_row(&self, index: i32) -> Option<&FileReport> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.file_rows.get(index))
    }

    fn volume_row(&self, index: i32) -> Option<&Volume> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.volumes.get(index))
    }

    fn plan_candidate(&self, index: i32) -> Option<&PlanCandidate> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.plan_candidates.get(index))
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
            let bytes = encode_map(&bins);
            let data = QByteArray::from(bytes.as_slice());
            let _ = qt_thread.queue(move |mut controller| {
                controller.as_mut().set_display_map_data(data);
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
                report,
            } => {
                let file_row_count = count_i32(report.file_rows.len());
                let report_volume_id = QString::from(&report.volume_id.0.to_string());
                let status = QString::from(&report.status);

                self.as_mut().rust_mut().analysis_id = Some(analysis_id);
                self.as_mut().rust_mut().map_bins = report.map_bins;
                self.as_mut().rust_mut().file_rows = report.file_rows;
                self.as_mut().rust_mut().worker = None;
                let map_revision = self.map_revision.wrapping_add(1);
                self.as_mut().set_map_revision(map_revision);
                self.as_mut().set_report_volume_id(report_volume_id);
                self.as_mut()
                    .set_fragmented_basis_points(report.fragmented_basis_points);
                self.as_mut()
                    .set_coverage_basis_points(report.coverage_basis_points);
                self.as_mut().set_file_row_count(file_row_count);
                self.as_mut().set_files_scanned(report.files_scanned);
                self.as_mut().set_bytes_scanned(report.bytes_scanned);
                self.as_mut().set_skipped_entries(report.skipped_entries);
                self.as_mut().set_has_report(true);
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut().set_status(status);
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

fn prepare_ui_report(mut report: Box<AnalysisReport>) -> UiReport {
    let status =
        if report.coverage.skipped_entries > 0 {
            report.warnings.last().cloned().unwrap_or_else(|| {
                "Analysis is partial because some entries were skipped.".to_owned()
            })
        } else {
            String::new()
        };
    report.files.retain(|file| file.excess_runs > 0);
    UiReport {
        volume_id: report.volume.id,
        fragmented_basis_points: optional_basis_points(
            report.fragmentation.fragmented_basis_points,
        ),
        coverage_basis_points: optional_basis_points(report.coverage.estimated_basis_points),
        files_scanned: report.coverage.files_scanned as f64,
        bytes_scanned: report.coverage.scanned_allocated_bytes as f64,
        skipped_entries: report.coverage.skipped_entries as f64,
        status,
        map_bins: report.map,
        file_rows: report.files,
    }
}

fn count_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn optional_basis_points(value: Option<u16>) -> i32 {
    value.map_or(-1, i32::from)
}

fn encode_map(bins: &[MapBin]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(bins.len().saturating_mul(MAP_RECORD_BYTES));
    for bin in bins {
        bytes.extend_from_slice(&bin.offset_bytes.to_le_bytes());
        bytes.extend_from_slice(&bin.length_bytes.to_le_bytes());
        for value in [
            bin.mix.free,
            bin.mix.contiguous_data,
            bin.mix.fragmented_data,
            bin.mix.unscanned_data,
        ]
        .into_iter()
        .chain(metadata_values(bin.mix.metadata))
        {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
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
    let columns = ((width.saturating_add(2)) / PITCH).max(1);
    let rows = ((height.saturating_add(2)) / PITCH).max(1);
    let available = usize::try_from(columns.saturating_mul(rows)).unwrap_or(usize::MAX);

    if source.is_empty() {
        if capacity == 0 {
            return Vec::new();
        }
        let capacity_limit = usize::try_from(capacity).unwrap_or(usize::MAX);
        let count = available.min(capacity_limit).max(1);
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

    let total_length = source
        .iter()
        .map(|bin| bin.length_bytes)
        .fold(0u64, u64::saturating_add);
    if total_length == 0 {
        return Vec::new();
    }
    let length_limit = usize::try_from(total_length).unwrap_or(usize::MAX);
    let count = available.min(length_limit).max(1);
    if count == source.len() {
        return source.to_vec();
    }

    resample_map(source, count, total_length)
}

fn resample_map(source: &[MapBin], count: usize, total_length: u64) -> Vec<MapBin> {
    let mut result = Vec::with_capacity(count);
    let mut source_index = 0usize;
    let mut source_start = 0u64;

    for target in 0..count {
        let target_start = partition_point(target, count, total_length);
        let target_end = partition_point(target + 1, count, total_length);

        while source_index + 1 < source.len()
            && source_start.saturating_add(source[source_index].length_bytes) <= target_start
        {
            source_start = source_start.saturating_add(source[source_index].length_bytes);
            source_index += 1;
        }

        let first_source = &source[source_index];
        let offset_bytes = first_source
            .offset_bytes
            .saturating_add(target_start.saturating_sub(source_start));
        let mut length = 0u64;
        let mut categories = [0u128; 4];
        let mut metadata = [0u128; 9];
        let mut cursor = target_start;
        let mut index = source_index;
        let mut bin_start = source_start;

        while cursor < target_end && index < source.len() {
            let bin = &source[index];
            let bin_end = bin_start.saturating_add(bin.length_bytes);
            let overlap_end = target_end.min(bin_end);
            let overlap = overlap_end.saturating_sub(cursor);
            let span = u128::from(overlap);
            length = length.saturating_add(overlap);
            categories[0] += u128::from(bin.mix.free) * span;
            categories[1] += u128::from(bin.mix.contiguous_data) * span;
            categories[2] += u128::from(bin.mix.fragmented_data) * span;
            categories[3] += u128::from(bin.mix.unscanned_data) * span;
            for (total, value) in metadata.iter_mut().zip(metadata_values(bin.mix.metadata)) {
                *total += u128::from(value) * span;
            }

            cursor = overlap_end;
            if cursor >= bin_end {
                bin_start = bin_end;
                index += 1;
            }
        }

        let scale = |value| weighted_basis_points(value, length);
        let metadata = metadata.map(scale);
        result.push(MapBin {
            offset_bytes,
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

        source_index = index.min(source.len().saturating_sub(1));
        source_start = if index < source.len() {
            bin_start
        } else {
            total_length.saturating_sub(source[source_index].length_bytes)
        };
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

    #[test]
    fn analyzed_map_expands_to_fill_the_grid() {
        let source = vec![
            MapBin {
                offset_bytes: 1_000,
                length_bytes: 50,
                mix: CategoryMix {
                    free: 10_000,
                    ..CategoryMix::default()
                },
            },
            MapBin {
                offset_bytes: 1_050,
                length_bytes: 50,
                mix: CategoryMix {
                    contiguous_data: 10_000,
                    ..CategoryMix::default()
                },
            },
        ];

        let bins = aggregate_map(&source, 100, 55, 22);

        assert_eq!(bins.len(), 10);
        assert_eq!(bins[0].offset_bytes, 1_000);
        assert_eq!(bins[9].offset_bytes, 1_090);
        assert!(bins[..5].iter().all(|bin| bin.mix.free == 10_000));
        assert!(
            bins[5..]
                .iter()
                .all(|bin| bin.mix.contiguous_data == 10_000)
        );
    }

    #[test]
    fn map_transport_is_fixed_width_binary() {
        let bin = MapBin {
            offset_bytes: 0x0102_0304_0506_0708,
            length_bytes: 0x1112_1314_1516_1718,
            mix: CategoryMix {
                free: 1,
                contiguous_data: 2,
                fragmented_data: 3,
                unscanned_data: 4,
                metadata: MetadataMix {
                    filesystem_headers: 5,
                    journal: 6,
                    allocation_tables: 7,
                    file_metadata: 8,
                    group_descriptors: 9,
                    block_bitmaps: 10,
                    file_bitmaps: 11,
                    reserved: 12,
                    other: 13,
                },
            },
        };

        let bytes = encode_map(&[bin]);

        assert_eq!(bytes.len(), MAP_RECORD_BYTES);
        assert_eq!(&bytes[..8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x1112_1314_1516_1718u64.to_le_bytes());
        for (index, value) in (1u16..=13).enumerate() {
            let offset = 16 + index * 2;
            assert_eq!(&bytes[offset..offset + 2], &value.to_le_bytes());
        }
    }
}
