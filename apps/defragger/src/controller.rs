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
        #[qproperty(QByteArray, activity_data)]
        #[qproperty(QString, status)]
        #[qproperty(QString, map_volume_id)]
        #[qproperty(QString, report_volume_id)]
        #[qproperty(QString, analyzing_volume_id)]
        #[qproperty(i32, volume_count)]
        #[qproperty(i32, map_revision)]
        #[qproperty(i32, analysis_revision)]
        #[qproperty(i32, display_map_generation)]
        #[qproperty(i32, activity_revision)]
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
        fn select_volume(self: Pin<&mut Controller>, volume_id: &QString);
        #[qinvokable]
        fn pause(self: Pin<&mut Controller>);
        #[qinvokable]
        fn resume(self: Pin<&mut Controller>);
        #[qinvokable]
        fn stop(self: Pin<&mut Controller>);
        #[qinvokable]
        fn build_plan(self: Pin<&mut Controller>);
        #[qinvokable]
        fn start_defrag(self: Pin<&mut Controller>);
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
        fn volume_has_report(self: &Controller, index: i32) -> bool;
        #[qinvokable]
        fn volume_fragmented_basis_points(self: &Controller, index: i32) -> i32;
        #[qinvokable]
        fn file_path(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn map_file_path(self: &Controller, index: i32) -> QString;
        #[qinvokable]
        fn file_size_bytes(self: &Controller, index: i32) -> f64;
        #[qinvokable]
        fn file_fragment_count(self: &Controller, index: i32) -> i32;
        #[qinvokable]
        fn file_average_fragment_bytes(self: &Controller, index: i32) -> f64;
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
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    time::Duration,
};

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QByteArray, QString};
use defrag_domain::{
    AnalysisId, AnalysisReport, CategoryMix, DefragPolicy, FileReport, MapBin, MetadataMix,
    PhysicalRange, PlanCandidate, PlanId, ServiceEvent, SupportStatus, Volume, VolumeId,
};
#[cfg(all(feature = "development-service", not(feature = "system-helper")))]
use defrag_service::DevelopmentClient as AppClient;
#[cfg(all(not(feature = "development-service"), not(feature = "system-helper")))]
use defrag_service::InProcessClient as AppClient;
#[cfg(feature = "system-helper")]
use defrag_service::PrivilegedClient as AppClient;

const MAP_CATEGORY_BYTES: usize = 44;
const MAP_CONTRIBUTOR_BYTES: usize = 6;
const MAX_MAP_CONTRIBUTORS: usize = 5;
const MAP_RECORD_BYTES: usize = MAP_CATEGORY_BYTES + MAP_CONTRIBUTOR_BYTES * MAX_MAP_CONTRIBUTORS;

#[derive(Clone, Copy)]
struct MapContributor {
    file_index: u32,
    coverage_basis_points: u16,
}

#[derive(Clone, Copy)]
struct MapFileRange {
    file_index: usize,
    physical: PhysicalRange,
}

impl Default for MapContributor {
    fn default() -> Self {
        Self {
            file_index: u32::MAX,
            coverage_basis_points: 0,
        }
    }
}

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
    Activity {
        reading: Vec<PhysicalRange>,
        writing: Vec<PhysicalRange>,
    },
    DefragFinished {
        report: UiReport,
        stopped: bool,
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
    map_files: Vec<FileReport>,
}

#[derive(Clone)]
struct CachedAnalysis {
    analysis_id: Option<AnalysisId>,
    fragmented_basis_points: i32,
    coverage_basis_points: i32,
    files_scanned: f64,
    bytes_scanned: f64,
    skipped_entries: f64,
    status: String,
    map_bins: Vec<MapBin>,
    file_rows: Vec<FileReport>,
    map_files: Vec<FileReport>,
    map_file_ranges: Arc<Vec<MapFileRange>>,
}

pub struct ControllerRust {
    display_map_data: QByteArray,
    activity_data: QByteArray,
    status: QString,
    map_volume_id: QString,
    report_volume_id: QString,
    analyzing_volume_id: QString,
    volume_count: i32,
    map_revision: i32,
    analysis_revision: i32,
    display_map_generation: i32,
    activity_revision: i32,
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
    client: Option<AppClient>,
    worker: Option<Sender<WorkerCommand>>,
    analysis_id: Option<AnalysisId>,
    plan_id: Option<PlanId>,
    visible_volume_id: Option<VolumeId>,
    active_volume_id: Option<VolumeId>,
    active_map_bins: Vec<MapBin>,
    active_files_scanned: f64,
    active_bytes_scanned: f64,
    active_status: String,
    analyses: HashMap<VolumeId, CachedAnalysis>,
    volumes: Vec<Volume>,
    map_bins: Vec<MapBin>,
    file_rows: Vec<FileReport>,
    map_files: Vec<FileReport>,
    map_file_ranges: Arc<Vec<MapFileRange>>,
    plan_candidates: Vec<PlanCandidate>,
}

impl Default for ControllerRust {
    fn default() -> Self {
        Self {
            display_map_data: QByteArray::default(),
            activity_data: QByteArray::default(),
            status: QString::default(),
            map_volume_id: QString::default(),
            report_volume_id: QString::default(),
            analyzing_volume_id: QString::default(),
            volume_count: 0,
            map_revision: 0,
            analysis_revision: 0,
            display_map_generation: 0,
            activity_revision: 0,
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
            client: None,
            worker: None,
            analysis_id: None,
            plan_id: None,
            visible_volume_id: None,
            active_volume_id: None,
            active_map_bins: Vec::new(),
            active_files_scanned: 0.0,
            active_bytes_scanned: 0.0,
            active_status: String::new(),
            analyses: HashMap::new(),
            volumes: Vec::new(),
            map_bins: Vec::new(),
            file_rows: Vec::new(),
            map_files: Vec::new(),
            map_file_ranges: Arc::default(),
            plan_candidates: Vec::new(),
        }
    }
}

#[cfg(all(feature = "development-service", not(feature = "system-helper")))]
fn connect_client() -> Result<AppClient, String> {
    AppClient::connect().map_err(|error| error.to_string())
}

#[cfg(all(not(feature = "development-service"), not(feature = "system-helper")))]
fn connect_client() -> Result<AppClient, String> {
    Ok(AppClient::new())
}

#[cfg(feature = "system-helper")]
fn connect_client() -> Result<AppClient, String> {
    AppClient::connect().map_err(|error| error.to_string())
}

#[cfg(all(not(feature = "development-service"), not(feature = "system-helper")))]
fn starting_status() -> &'static str {
    "Reading the filesystem allocation map…"
}

#[cfg(any(feature = "development-service", feature = "system-helper"))]
fn starting_status() -> &'static str {
    "Waiting for authorization…"
}

impl qobject::Controller {
    fn refresh(mut self: Pin<&mut Self>) {
        let client = match self.client.as_ref().cloned() {
            Some(client) => client,
            None => match connect_client() {
                Ok(client) => client,
                Err(error) => {
                    self.as_mut().set_status(QString::from(&format!(
                        "Could not initialize analysis: {error}"
                    )));
                    return;
                }
            },
        };
        match client.list_volumes() {
            Ok(volumes) => {
                let count = count_i32(volumes.len());
                self.as_mut().set_volume_count(0);
                self.as_mut().rust_mut().volumes = volumes;
                self.as_mut().rust_mut().client = Some(client);
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
        let Some(client) = self.client.as_ref().cloned() else {
            self.as_mut().set_status(QString::from(
                "Refresh volumes to initialize the analysis service",
            ));
            return;
        };

        let volume_id = VolumeId(volume_id);
        let (command_sender, command_receiver) = mpsc::channel();
        self.as_mut().rust_mut().worker = Some(command_sender);
        self.as_mut().rust_mut().active_volume_id = Some(volume_id);
        self.as_mut().rust_mut().active_map_bins.clear();
        self.as_mut().rust_mut().active_files_scanned = 0.0;
        self.as_mut().rust_mut().active_bytes_scanned = 0.0;
        self.as_mut().rust_mut().active_status = starting_status().to_owned();
        self.as_mut()
            .set_analyzing_volume_id(QString::from(&volume_id.0.to_string()));
        self.as_mut().set_paused(false);
        self.as_mut().set_busy(true);
        self.as_mut().display_volume(volume_id);

        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let handle = match client.start_analysis(volume_id) {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = qt_thread.queue(move |controller| {
                        controller.apply_update(UiUpdate::Failed(error.to_string()))
                    });
                    return;
                }
            };
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

    fn select_volume(mut self: Pin<&mut Self>, volume_id: &QString) {
        let Ok(volume_id) = volume_id.to_string().parse::<u64>() else {
            self.as_mut().clear_display();
            self.as_mut().rust_mut().visible_volume_id = None;
            return;
        };
        self.as_mut().display_volume(VolumeId(volume_id));
    }

    fn pause(mut self: Pin<&mut Self>) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(WorkerCommand::Pause);
            self.as_mut().rust_mut().active_status = "Analysis paused".to_owned();
            self.as_mut().set_paused(true);
            if self.active_is_visible() {
                self.as_mut().set_status(QString::from("Analysis paused"));
            }
        }
    }

    fn resume(mut self: Pin<&mut Self>) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(WorkerCommand::Resume);
            self.as_mut().rust_mut().active_status = "Analysis resumed".to_owned();
            self.as_mut().set_paused(false);
            if self.active_is_visible() {
                self.as_mut().set_status(QString::from("Analysis resumed"));
            }
        }
    }

    fn stop(mut self: Pin<&mut Self>) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(WorkerCommand::Cancel);
            self.as_mut().rust_mut().active_status = "Stopping analysis…".to_owned();
            if self.active_is_visible() {
                self.as_mut()
                    .set_status(QString::from("Stopping analysis…"));
            }
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
        let Some(client) = self.client.as_ref().cloned() else {
            self.as_mut()
                .set_status(QString::from("The analysis session is unavailable"));
            return;
        };
        match client.build_plan(analysis_id, &policy) {
            Ok((plan_id, plan)) => {
                let count = count_i32(plan.candidates.len());
                let estimated_rewrite_bytes = plan.estimated_rewrite_bytes as f64;
                self.as_mut().rust_mut().plan_candidates = plan.candidates;
                self.as_mut().rust_mut().plan_id = Some(plan_id);
                self.as_mut().set_plan_candidate_count(count);
                self.as_mut()
                    .set_plan_estimated_rewrite_bytes(estimated_rewrite_bytes);
                let revision = self.plan_revision.wrapping_add(1).max(1);
                self.as_mut().set_plan_revision(revision);
                self.as_mut()
                    .set_status(QString::from("Defragmentation plan ready"));
            }
            Err(error) => self
                .as_mut()
                .set_status(QString::from(&format!("Could not build plan: {error}"))),
        }
    }

    fn start_defrag(mut self: Pin<&mut Self>) {
        if self.busy {
            return;
        }
        let Some(plan_id) = self.as_mut().rust_mut().plan_id.take() else {
            self.as_mut()
                .set_status(QString::from("Build a defragmentation plan first"));
            return;
        };
        let Some(client) = self.client.as_ref().cloned() else {
            self.as_mut()
                .set_status(QString::from("The helper is unavailable"));
            return;
        };
        let Some(volume_id) = self.visible_volume_id else {
            return;
        };
        let (command_sender, command_receiver) = mpsc::channel();
        self.as_mut().rust_mut().worker = Some(command_sender);
        self.as_mut().rust_mut().active_volume_id = Some(volume_id);
        self.as_mut().rust_mut().active_map_bins = self.map_bins.clone();
        self.as_mut().set_busy(true);
        self.as_mut().set_paused(false);
        self.as_mut()
            .set_analyzing_volume_id(QString::from(&volume_id.0.to_string()));
        self.as_mut()
            .set_status(QString::from("Starting defragmentation…"));

        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let handle = match client.start_defrag(plan_id) {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = qt_thread.queue(move |controller| {
                        controller.apply_update(UiUpdate::Failed(error.to_string()))
                    });
                    return;
                }
            };
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
                    Ok(ServiceEvent::DefragActivity {
                        reading, writing, ..
                    }) => Some(UiUpdate::Activity { reading, writing }),
                    Ok(ServiceEvent::DefragProgress(progress)) => Some(UiUpdate::Progress {
                        files: progress.files_completed,
                        bytes: progress.bytes_moved,
                        detail: progress
                            .current_path
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| format!("{:?}", progress.phase)),
                    }),
                    Ok(ServiceEvent::DefragFinished { report, .. }) => {
                        Some(UiUpdate::DefragFinished {
                            report: prepare_ui_report(report),
                            stopped: false,
                        })
                    }
                    Ok(ServiceEvent::DefragStopped { report, .. }) => {
                        Some(UiUpdate::DefragFinished {
                            report: prepare_ui_report(report),
                            stopped: true,
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
                        UiUpdate::DefragFinished { .. } | UiUpdate::Cancelled | UiUpdate::Failed(_)
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

    fn volume_id(&self, index: i32) -> QString {
        self.volume_row(index)
            .map_or_else(QString::default, |volume| {
                QString::from(&volume.id.0.to_string())
            })
    }

    fn volume_mount_point(&self, index: i32) -> QString {
        self.volume_row(index)
            .map_or_else(QString::default, |volume| {
                QString::from(
                    &volume
                        .mount_point
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "Unmounted".to_owned()),
                )
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
            .and_then(|volume| volume.used_bytes)
            .map_or(0.0, |bytes| bytes as f64)
    }

    fn volume_free_bytes(&self, index: i32) -> f64 {
        self.volume_row(index)
            .and_then(|volume| volume.free_bytes)
            .map_or(0.0, |bytes| bytes as f64)
    }

    fn volume_supported(&self, index: i32) -> bool {
        self.volume_row(index).is_some_and(|volume| {
            matches!(
                volume.support,
                SupportStatus::ReadOnly | SupportStatus::Defragmentable
            )
        })
    }

    fn volume_has_report(&self, index: i32) -> bool {
        self.volume_row(index)
            .is_some_and(|volume| self.analyses.contains_key(&volume.id))
    }

    fn volume_fragmented_basis_points(&self, index: i32) -> i32 {
        self.volume_row(index)
            .and_then(|volume| self.analyses.get(&volume.id))
            .map_or(-1, |analysis| analysis.fragmented_basis_points)
    }

    fn file_path(&self, index: i32) -> QString {
        self.file_row(index).map_or_else(QString::default, |file| {
            QString::from(&file.path.display().to_string())
        })
    }

    fn map_file_path(&self, index: i32) -> QString {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.map_files.get(index))
            .map_or_else(QString::default, |file| {
                QString::from(&file.path.display().to_string())
            })
    }

    fn file_size_bytes(&self, index: i32) -> f64 {
        self.file_row(index)
            .map_or(0.0, |file| file.logical_bytes as f64)
    }

    fn file_fragment_count(&self, index: i32) -> i32 {
        self.file_row(index)
            .map_or(0, |file| count_i32(file.physical_runs as usize))
    }

    fn file_average_fragment_bytes(&self, index: i32) -> f64 {
        self.file_row(index)
            .map_or(0.0, |file| file.average_run_bytes as f64)
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

    fn clear_display(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().analysis_id = None;
        self.as_mut().rust_mut().plan_id = None;
        self.as_mut().rust_mut().map_bins.clear();
        self.as_mut().rust_mut().file_rows.clear();
        self.as_mut().rust_mut().map_files.clear();
        self.as_mut().rust_mut().map_file_ranges = Arc::default();
        self.as_mut().rust_mut().plan_candidates.clear();
        self.as_mut().set_map_volume_id(QString::default());
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
        self.as_mut().set_status(QString::default());
        let revision = self.map_revision.wrapping_add(1);
        self.as_mut().set_map_revision(revision);
    }

    fn display_volume(mut self: Pin<&mut Self>, volume_id: VolumeId) {
        self.as_mut().rust_mut().visible_volume_id = Some(volume_id);
        let active = (self.active_volume_id == Some(volume_id)).then(|| {
            (
                self.active_map_bins.clone(),
                self.active_files_scanned,
                self.active_bytes_scanned,
                self.active_status.clone(),
            )
        });
        let cached = self.analyses.get(&volume_id).cloned();
        self.as_mut().clear_display();

        let volume_id_string = QString::from(&volume_id.0.to_string());
        if let Some((map_bins, files, bytes, status)) = active {
            self.as_mut().rust_mut().map_bins = map_bins;
            self.as_mut().set_map_volume_id(volume_id_string);
            self.as_mut().set_files_scanned(files);
            self.as_mut().set_bytes_scanned(bytes);
            self.as_mut().set_status(QString::from(&status));
            return;
        }

        let Some(cached) = cached else {
            return;
        };
        let file_row_count = count_i32(cached.file_rows.len());
        self.as_mut().rust_mut().analysis_id = cached.analysis_id;
        self.as_mut().rust_mut().map_bins = cached.map_bins;
        self.as_mut().rust_mut().file_rows = cached.file_rows;
        self.as_mut().rust_mut().map_files = cached.map_files;
        self.as_mut().rust_mut().map_file_ranges = cached.map_file_ranges;
        self.as_mut().set_map_volume_id(volume_id_string.clone());
        self.as_mut().set_report_volume_id(volume_id_string);
        self.as_mut()
            .set_fragmented_basis_points(cached.fragmented_basis_points);
        self.as_mut()
            .set_coverage_basis_points(cached.coverage_basis_points);
        self.as_mut().set_file_row_count(file_row_count);
        self.as_mut().set_files_scanned(cached.files_scanned);
        self.as_mut().set_bytes_scanned(cached.bytes_scanned);
        self.as_mut().set_skipped_entries(cached.skipped_entries);
        self.as_mut().set_has_report(true);
        self.as_mut().set_status(QString::from(&cached.status));
    }

    fn active_is_visible(&self) -> bool {
        self.active_volume_id.is_some() && self.active_volume_id == self.visible_volume_id
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
        let file_ranges = if use_analysis {
            Arc::clone(&self.map_file_ranges)
        } else {
            Arc::default()
        };
        let width = dimension(width);
        let height = dimension(height);
        let capacity_bytes = finite_u64(capacity_bytes);
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let bins = aggregate_map(&source, capacity_bytes, width, height);
            let contributors = file_contributors(&bins, &file_ranges);
            let bytes = encode_map(&bins, &contributors);
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
                merge_map_bins(&mut self.as_mut().rust_mut().active_map_bins, full, bins);
                if self.active_is_visible() {
                    self.as_mut().rust_mut().map_bins = self.active_map_bins.clone();
                    let revision = self.map_revision.wrapping_add(1);
                    self.as_mut().set_map_revision(revision);
                }
            }
            UiUpdate::Progress {
                files,
                bytes,
                detail,
            } => {
                self.as_mut().rust_mut().active_files_scanned = files as f64;
                self.as_mut().rust_mut().active_bytes_scanned = bytes as f64;
                self.as_mut().rust_mut().active_status = detail.clone();
                if self.active_is_visible() {
                    self.as_mut().set_files_scanned(files as f64);
                    self.as_mut().set_bytes_scanned(bytes as f64);
                    self.as_mut().set_status(QString::from(&detail));
                }
            }
            UiUpdate::Finished {
                analysis_id,
                report,
            } => {
                let (volume_id, cached) = cache_report(report, Some(analysis_id));
                self.as_mut().rust_mut().analyses.insert(volume_id, cached);
                let analysis_revision = self.analysis_revision.wrapping_add(1);
                self.as_mut().set_analysis_revision(analysis_revision);
                self.as_mut().rust_mut().worker = None;
                self.as_mut().rust_mut().active_volume_id = None;
                self.as_mut().rust_mut().active_map_bins.clear();
                self.as_mut().rust_mut().active_status.clear();
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut().set_analyzing_volume_id(QString::default());
                if self.visible_volume_id == Some(volume_id) {
                    self.as_mut().display_volume(volume_id);
                }
            }
            UiUpdate::Activity { reading, writing } => {
                let data = encode_activity(&reading, &writing);
                self.as_mut()
                    .set_activity_data(QByteArray::from(data.as_slice()));
                let revision = self.activity_revision.wrapping_add(1);
                self.as_mut().set_activity_revision(revision);
            }
            UiUpdate::DefragFinished { report, stopped } => {
                let (volume_id, cached) = cache_report(report, None);
                self.as_mut().rust_mut().analyses.insert(volume_id, cached);
                self.as_mut().rust_mut().worker = None;
                self.as_mut().rust_mut().active_volume_id = None;
                self.as_mut().rust_mut().active_map_bins.clear();
                self.as_mut().rust_mut().active_status.clear();
                self.as_mut().set_activity_data(QByteArray::default());
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut().set_analyzing_volume_id(QString::default());
                let analysis_revision = self.analysis_revision.wrapping_add(1);
                self.as_mut().set_analysis_revision(analysis_revision);
                if self.visible_volume_id == Some(volume_id) {
                    self.as_mut().display_volume(volume_id);
                    self.as_mut().set_status(QString::from(if stopped {
                        "Defragmentation stopped safely"
                    } else {
                        "Defragmentation complete"
                    }));
                }
            }
            UiUpdate::Cancelled => {
                let active_volume_id = self.active_volume_id;
                self.as_mut().rust_mut().worker = None;
                self.as_mut().rust_mut().active_volume_id = None;
                self.as_mut().rust_mut().active_map_bins.clear();
                self.as_mut().rust_mut().active_status.clear();
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut().set_analyzing_volume_id(QString::default());
                self.as_mut().set_activity_data(QByteArray::default());
                if let Some(volume_id) = active_volume_id
                    && Some(volume_id) == self.visible_volume_id
                {
                    self.as_mut().display_volume(volume_id);
                    self.as_mut()
                        .set_status(QString::from("Analysis cancelled"));
                }
            }
            UiUpdate::Failed(message) => {
                let active_volume_id = self.active_volume_id;
                self.as_mut().rust_mut().worker = None;
                self.as_mut().rust_mut().active_volume_id = None;
                self.as_mut().rust_mut().active_map_bins.clear();
                self.as_mut().rust_mut().active_status.clear();
                self.as_mut().set_busy(false);
                self.as_mut().set_paused(false);
                self.as_mut().set_analyzing_volume_id(QString::default());
                self.as_mut().set_activity_data(QByteArray::default());
                if let Some(volume_id) = active_volume_id
                    && Some(volume_id) == self.visible_volume_id
                {
                    self.as_mut().display_volume(volume_id);
                    self.as_mut()
                        .set_status(QString::from(&format!("Analysis failed: {message}")));
                }
            }
        }
    }
}

fn cache_report(report: UiReport, analysis_id: Option<AnalysisId>) -> (VolumeId, CachedAnalysis) {
    let volume_id = report.volume_id;
    let ranges = report
        .map_files
        .iter()
        .enumerate()
        .flat_map(|(file_index, file)| {
            file.physical_ranges
                .iter()
                .copied()
                .map(move |physical| MapFileRange {
                    file_index,
                    physical,
                })
        })
        .collect();
    let mut map_files = report.map_files;
    for file in &mut map_files {
        file.physical_ranges.clear();
    }
    (
        volume_id,
        CachedAnalysis {
            analysis_id,
            fragmented_basis_points: report.fragmented_basis_points,
            coverage_basis_points: report.coverage_basis_points,
            files_scanned: report.files_scanned,
            bytes_scanned: report.bytes_scanned,
            skipped_entries: report.skipped_entries,
            status: report.status,
            map_bins: report.map_bins,
            file_rows: report.file_rows,
            map_files,
            map_file_ranges: Arc::new(ranges),
        },
    )
}

fn encode_activity(reading: &[PhysicalRange], writing: &[PhysicalRange]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((reading.len() + writing.len()) * 24);
    for (kind, ranges) in [(1u8, reading), (2u8, writing)] {
        for range in ranges {
            bytes.extend_from_slice(&range.offset_bytes.to_le_bytes());
            bytes.extend_from_slice(&range.length_bytes.to_le_bytes());
            bytes.push(kind);
            bytes.extend_from_slice(&[0; 7]);
        }
    }
    bytes
}

fn merge_map_bins(target: &mut Vec<MapBin>, full: bool, bins: Vec<MapBin>) {
    if full {
        *target = bins;
        return;
    }
    for bin in bins {
        match target.binary_search_by_key(&bin.offset_bytes, |item| item.offset_bytes) {
            Ok(index) => target[index] = bin,
            Err(index) => target.insert(index, bin),
        }
    }
}

fn prepare_ui_report(report: Box<AnalysisReport>) -> UiReport {
    let status =
        if report.coverage.skipped_entries > 0 {
            report.warnings.last().cloned().unwrap_or_else(|| {
                "Analysis is partial because some entries were skipped.".to_owned()
            })
        } else {
            String::new()
        };
    let map_files = report.files;
    let file_rows = map_files
        .iter()
        .filter(|file| file.excess_runs > 0)
        .cloned()
        .map(|mut file| {
            file.physical_ranges.clear();
            file
        })
        .collect();
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
        file_rows,
        map_files,
    }
}

fn count_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn optional_basis_points(value: Option<u16>) -> i32 {
    value.map_or(-1, i32::from)
}

fn encode_map(bins: &[MapBin], contributors: &[[MapContributor; MAX_MAP_CONTRIBUTORS]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(bins.len().saturating_mul(MAP_RECORD_BYTES));
    for (index, bin) in bins.iter().enumerate() {
        bytes.extend_from_slice(&bin.offset_bytes.to_le_bytes());
        bytes.extend_from_slice(&bin.length_bytes.to_le_bytes());
        for value in [
            bin.mix.free,
            bin.mix.contiguous_data,
            bin.mix.fragmented_data,
            bin.mix.unscanned_data,
            bin.mix.defrag_staging,
        ]
        .into_iter()
        .chain(metadata_values(bin.mix.metadata))
        {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for contributor in contributors.get(index).copied().unwrap_or_default() {
            bytes.extend_from_slice(&contributor.file_index.to_le_bytes());
            bytes.extend_from_slice(&contributor.coverage_basis_points.to_le_bytes());
        }
    }
    bytes
}

fn file_contributors(
    bins: &[MapBin],
    file_ranges: &[MapFileRange],
) -> Vec<[MapContributor; MAX_MAP_CONTRIBUTORS]> {
    let mut overlaps = vec![Vec::<(usize, u64)>::new(); bins.len()];

    for entry in file_ranges {
        if u32::try_from(entry.file_index).is_err() || entry.physical.length_bytes == 0 {
            continue;
        }
        let range = entry.physical;
        let range_end = range.offset_bytes.saturating_add(range.length_bytes);
        let mut bin_index = bins.partition_point(|bin| {
            bin.offset_bytes.saturating_add(bin.length_bytes) <= range.offset_bytes
        });
        while let Some(bin) = bins.get(bin_index) {
            if bin.offset_bytes >= range_end {
                break;
            }
            let overlap = range_end
                .min(bin.offset_bytes.saturating_add(bin.length_bytes))
                .saturating_sub(range.offset_bytes.max(bin.offset_bytes));
            if overlap > 0 {
                overlaps[bin_index].push((entry.file_index, overlap));
            }
            bin_index += 1;
        }
    }

    overlaps
        .into_iter()
        .zip(bins)
        .map(|(mut entries, bin)| {
            entries.sort_unstable_by_key(|(file_index, _)| *file_index);
            let mut totals = Vec::<(usize, u64)>::new();
            for (file_index, overlap) in entries {
                if let Some((last_index, total)) = totals.last_mut()
                    && *last_index == file_index
                {
                    *total = total.saturating_add(overlap);
                } else {
                    totals.push((file_index, overlap));
                }
            }
            totals.sort_unstable_by(|left, right| {
                right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0))
            });

            let mut result = [MapContributor::default(); MAX_MAP_CONTRIBUTORS];
            for (slot, (file_index, overlap)) in result
                .iter_mut()
                .zip(totals.into_iter().take(MAX_MAP_CONTRIBUTORS))
            {
                *slot = MapContributor {
                    file_index: file_index as u32,
                    coverage_basis_points: weighted_basis_points(
                        u128::from(overlap) * 10_000,
                        bin.length_bytes,
                    )
                    .max(1),
                };
            }
            result
        })
        .collect()
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
        let mut categories = [0u128; 5];
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
            categories[4] += u128::from(bin.mix.defrag_staging) * span;
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
                defrag_staging: scale(categories[4]),
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
    fn contributors_are_ranked_by_overlap_with_each_block() {
        let bins = vec![
            MapBin {
                offset_bytes: 0,
                length_bytes: 100,
                mix: CategoryMix::default(),
            },
            MapBin {
                offset_bytes: 100,
                length_bytes: 100,
                mix: CategoryMix::default(),
            },
        ];
        let file_ranges = vec![
            MapFileRange {
                file_index: 0,
                physical: PhysicalRange {
                    offset_bytes: 10,
                    length_bytes: 80,
                },
            },
            MapFileRange {
                file_index: 1,
                physical: PhysicalRange {
                    offset_bytes: 0,
                    length_bytes: 30,
                },
            },
            MapFileRange {
                file_index: 1,
                physical: PhysicalRange {
                    offset_bytes: 100,
                    length_bytes: 50,
                },
            },
        ];

        let contributors = file_contributors(&bins, &file_ranges);

        assert_eq!(contributors[0][0].file_index, 0);
        assert_eq!(contributors[0][0].coverage_basis_points, 8_000);
        assert_eq!(contributors[0][1].file_index, 1);
        assert_eq!(contributors[0][1].coverage_basis_points, 3_000);
        assert_eq!(contributors[1][0].file_index, 1);
        assert_eq!(contributors[1][0].coverage_basis_points, 5_000);
        assert_eq!(contributors[1][1].file_index, u32::MAX);
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
                defrag_staging: 14,
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

        let mut contributors = [MapContributor::default(); MAX_MAP_CONTRIBUTORS];
        contributors[0] = MapContributor {
            file_index: 7,
            coverage_basis_points: 2_500,
        };
        let bytes = encode_map(&[bin], &[contributors]);

        assert_eq!(bytes.len(), MAP_RECORD_BYTES);
        assert_eq!(&bytes[..8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x1112_1314_1516_1718u64.to_le_bytes());
        for (index, value) in [1u16, 2, 3, 4, 14, 5, 6, 7, 8, 9, 10, 11, 12, 13]
            .into_iter()
            .enumerate()
        {
            let offset = 16 + index * 2;
            assert_eq!(&bytes[offset..offset + 2], &value.to_le_bytes());
        }
        assert_eq!(&bytes[44..48], &7u32.to_le_bytes());
        assert_eq!(&bytes[48..50], &2_500u16.to_le_bytes());
        assert_eq!(&bytes[50..54], &u32::MAX.to_le_bytes());
    }
}
