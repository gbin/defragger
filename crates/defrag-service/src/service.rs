use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

use defrag_domain::{
    AnalysisId, DefragPolicy, DefragProgress, FragmentationMetrics, JobId, JobProgress,
    OptimizationMode, PhysicalRange, PlanId, PlanSummary, ServiceEvent, SupportStatus, Volume,
    VolumeId,
};
use thiserror::Error;

use crate::{
    AnalysisAccess, EventSink, FilesystemAnalysis, FilesystemBackend, JobControl, default_backends,
    mounts,
};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Kernel(#[from] crate::linux::IoctlError),
    #[error("volume {0:?} is no longer mounted")]
    VolumeNotFound(VolumeId),
    #[error("filesystem {0} is not supported")]
    UnsupportedFilesystem(String),
    #[error("analysis {0:?} was not found")]
    AnalysisNotFound(AnalysisId),
    #[error("plan {0:?} was not found")]
    PlanNotFound(PlanId),
    #[error("defragmentation execution is unavailable for this plan")]
    ExecutionUnavailable,
    #[error("automatic unmount is unavailable: {0}")]
    UnmountUnavailable(String),
    #[error("{mode:?} optimization is not supported for {filesystem}")]
    UnsupportedOptimizationMode {
        filesystem: String,
        mode: OptimizationMode,
    },
    #[error("unsafe or inconsistent filesystem: {0}")]
    UnsafeFilesystem(String),
    #[error("job was cancelled")]
    Cancelled,
    #[error("service state was poisoned")]
    Poisoned,
}

struct Inner {
    backends: Vec<Arc<dyn FilesystemBackend>>,
    analyses: Mutex<HashMap<AnalysisId, Box<dyn FilesystemAnalysis>>>,
    plans: Mutex<HashMap<PlanId, Box<dyn crate::PreparedPlan>>>,
    next_job: AtomicU64,
    next_analysis: AtomicU64,
    next_plan: AtomicU64,
}

#[derive(Clone)]
pub struct InProcessClient {
    inner: Arc<Inner>,
}

impl Default for InProcessClient {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcessClient {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                backends: default_backends(),
                analyses: Mutex::new(HashMap::new()),
                plans: Mutex::new(HashMap::new()),
                next_job: AtomicU64::new(1),
                next_analysis: AtomicU64::new(1),
                next_plan: AtomicU64::new(1),
            }),
        }
    }

    pub fn list_volumes(&self) -> Result<Vec<Volume>, ServiceError> {
        mounts::discover(&self.inner.backends)
    }

    pub fn unmount_volume(&self, volume_id: VolumeId) -> Result<(), ServiceError> {
        let volume = self
            .list_volumes()?
            .into_iter()
            .find(|volume| volume.id == volume_id)
            .ok_or(ServiceError::VolumeNotFound(volume_id))?;
        if !matches!(volume.filesystem.as_str(), "fat" | "msdos" | "vfat") {
            return Err(ServiceError::UnmountUnavailable(format!(
                "{} does not require offline FAT access",
                volume.filesystem
            )));
        }
        mounts::unmount(&volume)
    }

    pub fn start_analysis(&self, volume_id: VolumeId) -> Result<JobHandle, ServiceError> {
        self.start_analysis_inner(volume_id, false)
    }

    /// Starts analysis and permits a private read-write mount solely when an
    /// offline ext4 journal must be replayed first. Callers must obtain
    /// modification authorization before selecting this path.
    pub fn start_analysis_allowing_recovery(
        &self,
        volume_id: VolumeId,
    ) -> Result<JobHandle, ServiceError> {
        self.start_analysis_inner(volume_id, true)
    }

    pub fn analysis_requires_recovery(&self, volume_id: VolumeId) -> Result<bool, ServiceError> {
        let volumes = self.list_volumes()?;
        let volume = volumes
            .into_iter()
            .find(|volume| volume.id == volume_id)
            .ok_or(ServiceError::VolumeNotFound(volume_id))?;
        mounts::ext4_needs_recovery(&volume)
    }

    fn start_analysis_inner(
        &self,
        volume_id: VolumeId,
        allow_journal_recovery: bool,
    ) -> Result<JobHandle, ServiceError> {
        let volumes = self.list_volumes()?;
        let volume = volumes
            .into_iter()
            .find(|volume| volume.id == volume_id)
            .ok_or(ServiceError::VolumeNotFound(volume_id))?;
        if !matches!(
            volume.support,
            SupportStatus::ReadOnly | SupportStatus::Defragmentable
        ) {
            return Err(ServiceError::UnsupportedFilesystem(volume.filesystem));
        }
        let backend = self
            .inner
            .backends
            .iter()
            .find(|backend| {
                matches!(
                    backend.probe(&volume),
                    SupportStatus::ReadOnly | SupportStatus::Defragmentable
                )
            })
            .cloned()
            .ok_or_else(|| ServiceError::UnsupportedFilesystem(volume.filesystem.clone()))?;

        let job_id = JobId(self.inner.next_job.fetch_add(1, Ordering::Relaxed));
        let control = Arc::new(Control::default());
        let (sender, receiver) = mpsc::channel();
        sender
            .send(ServiceEvent::AnalysisStarted { job_id })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "analysis event receiver closed")
            })?;

        let inner = Arc::clone(&self.inner);
        let thread_control = Arc::clone(&control);
        std::thread::Builder::new()
            .name(format!("defrag-analysis-{}", job_id.0))
            .spawn(move || {
                let sink = ChannelSink {
                    job_id,
                    sender: sender.clone(),
                };
                let mounted = if backend.analysis_access(&volume) == AnalysisAccess::RawDevice {
                    None
                } else {
                    match mounts::mount_for_analysis(&volume, allow_journal_recovery) {
                        Ok(mounted) => Some(mounted),
                        Err(error) => {
                            let _ = sender.send(ServiceEvent::Failed {
                                job_id: Some(job_id),
                                message: error.to_string(),
                            });
                            return;
                        }
                    }
                };
                let analysis_volume = mounted.as_ref().map_or(&volume, |mounted| &mounted.volume);
                match backend.analyze(analysis_volume, job_id, thread_control.as_ref(), &sink) {
                    Ok(analysis) => {
                        if volume.mount_state == defrag_domain::MountState::Unmounted
                            && volume.filesystem == "ext4"
                            && !analysis_volume.read_only
                            && let Err(error) = mounts::validate_ext4_writable(&volume)
                        {
                            let _ = sender.send(ServiceEvent::Failed {
                                job_id: Some(job_id),
                                message: error.to_string(),
                            });
                            return;
                        }
                        let analysis_id =
                            AnalysisId(inner.next_analysis.fetch_add(1, Ordering::Relaxed));
                        let report = analysis.report().clone();
                        match inner.analyses.lock() {
                            Ok(mut analyses) => {
                                analyses.insert(analysis_id, analysis);
                                let _ = sender.send(ServiceEvent::AnalysisFinished {
                                    job_id,
                                    analysis_id,
                                    report: Box::new(report),
                                });
                            }
                            Err(_) => {
                                let _ = sender.send(ServiceEvent::Failed {
                                    job_id: Some(job_id),
                                    message: ServiceError::Poisoned.to_string(),
                                });
                            }
                        }
                    }
                    Err(ServiceError::Cancelled) => {
                        let _ = sender.send(ServiceEvent::JobCancelled { job_id });
                    }
                    Err(error) => {
                        let _ = sender.send(ServiceEvent::Failed {
                            job_id: Some(job_id),
                            message: error.to_string(),
                        });
                    }
                }
            })?;

        Ok(JobHandle {
            job_id,
            receiver,
            control,
        })
    }

    pub fn build_plan(
        &self,
        analysis_id: AnalysisId,
        policy: &DefragPolicy,
    ) -> Result<(PlanId, PlanSummary), ServiceError> {
        let analyses = self
            .inner
            .analyses
            .lock()
            .map_err(|_| ServiceError::Poisoned)?;
        let analysis = analyses
            .get(&analysis_id)
            .ok_or(ServiceError::AnalysisNotFound(analysis_id))?;
        let plan = analysis.build_plan(policy)?;
        let plan_id = PlanId(self.inner.next_plan.fetch_add(1, Ordering::Relaxed));
        let summary = plan.summary().clone();
        self.inner
            .plans
            .lock()
            .map_err(|_| ServiceError::Poisoned)?
            .insert(plan_id, plan);
        Ok((plan_id, summary))
    }

    pub fn start_defrag(&self, plan_id: PlanId) -> Result<JobHandle, ServiceError> {
        let plan = self
            .inner
            .plans
            .lock()
            .map_err(|_| ServiceError::Poisoned)?
            .remove(&plan_id)
            .ok_or(ServiceError::PlanNotFound(plan_id))?;
        if !plan.execution_requirements().available_in_this_build {
            return Err(ServiceError::ExecutionUnavailable);
        }
        let job_id = JobId(self.inner.next_job.fetch_add(1, Ordering::Relaxed));
        let control = Arc::new(Control::default());
        let (sender, receiver) = mpsc::channel();
        sender
            .send(ServiceEvent::DefragStarted { job_id, plan_id })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "defrag event receiver closed")
            })?;
        let thread_control = Arc::clone(&control);
        std::thread::Builder::new()
            .name(format!("defrag-execute-{}", job_id.0))
            .spawn(move || {
                let sink = ChannelSink {
                    job_id,
                    sender: sender.clone(),
                };
                match plan.execute(job_id, thread_control.as_ref(), &sink) {
                    Ok(execution) => {
                        let event = if execution.stopped {
                            ServiceEvent::DefragStopped {
                                job_id,
                                report: Box::new(execution.report),
                            }
                        } else {
                            ServiceEvent::DefragFinished {
                                job_id,
                                report: Box::new(execution.report),
                            }
                        };
                        let _ = sender.send(event);
                    }
                    Err(ServiceError::Cancelled) => {
                        let _ = sender.send(ServiceEvent::JobCancelled { job_id });
                    }
                    Err(error) => {
                        let _ = sender.send(ServiceEvent::Failed {
                            job_id: Some(job_id),
                            message: error.to_string(),
                        });
                    }
                }
            })?;
        Ok(JobHandle {
            job_id,
            receiver,
            control,
        })
    }

    pub fn discard_analysis(&self, analysis_id: AnalysisId) -> Result<(), ServiceError> {
        self.inner
            .analyses
            .lock()
            .map_err(|_| ServiceError::Poisoned)?
            .remove(&analysis_id);
        Ok(())
    }
}

pub struct JobHandle {
    job_id: JobId,
    receiver: Receiver<ServiceEvent>,
    control: Arc<Control>,
}

impl JobHandle {
    pub fn id(&self) -> JobId {
        self.job_id
    }

    pub fn events(&self) -> &Receiver<ServiceEvent> {
        &self.receiver
    }

    pub fn pause(&self) {
        self.control.set_paused(true);
    }

    pub fn resume(&self) {
        self.control.set_paused(false);
    }

    pub fn cancel(&self) {
        self.control.cancelled.store(true, Ordering::Release);
        self.control.waiter.notify_all();
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Default)]
struct Control {
    paused: Mutex<bool>,
    waiter: Condvar,
    cancelled: AtomicBool,
}

impl Control {
    fn set_paused(&self, paused: bool) {
        if let Ok(mut state) = self.paused.lock() {
            *state = paused;
            if !paused {
                self.waiter.notify_all();
            }
        }
    }
}

impl JobControl for Control {
    fn checkpoint(&self) -> Result<(), ServiceError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(ServiceError::Cancelled);
        }
        let mut paused = self.paused.lock().map_err(|_| ServiceError::Poisoned)?;
        while *paused && !self.cancelled.load(Ordering::Acquire) {
            paused = self
                .waiter
                .wait(paused)
                .map_err(|_| ServiceError::Poisoned)?;
        }
        if self.cancelled.load(Ordering::Acquire) {
            Err(ServiceError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

struct ChannelSink {
    job_id: JobId,
    sender: Sender<ServiceEvent>,
}

impl EventSink for ChannelSink {
    fn progress(&self, mut progress: JobProgress) {
        progress.job_id = self.job_id;
        let _ = self.sender.send(ServiceEvent::Progress(progress));
    }

    fn map_updated(&self, full_snapshot: bool, bins: Vec<defrag_domain::MapBin>) {
        let _ = self.sender.send(ServiceEvent::MapUpdated {
            job_id: self.job_id,
            full_snapshot,
            bins,
        });
    }

    fn defrag_progress(&self, mut progress: DefragProgress) {
        progress.job_id = self.job_id;
        let _ = self.sender.send(ServiceEvent::DefragProgress(progress));
    }

    fn defrag_activity(&self, reading: Vec<PhysicalRange>, writing: Vec<PhysicalRange>) {
        let _ = self.sender.send(ServiceEvent::DefragActivity {
            job_id: self.job_id,
            reading,
            writing,
        });
    }

    fn defrag_file_updated(
        &self,
        file: defrag_domain::FileReport,
        fragmentation: FragmentationMetrics,
        bytes_moved: u64,
    ) {
        let _ = self.sender.send(ServiceEvent::DefragFileUpdated {
            job_id: self.job_id,
            file,
            fragmentation,
            bytes_moved,
        });
    }
}
