mod block_map;
mod classic_fat;
mod ext4;
mod fat;
mod linux;
mod mounts;
#[cfg(feature = "system-helper-client")]
mod remote;
mod service;

use std::sync::Arc;

use defrag_domain::{
    AnalysisReport, DefragPolicy, DefragProgress, ExecutionRequirements, PhysicalRange,
    PlanSummary, SupportStatus, Volume,
};

#[cfg(feature = "development-client")]
pub use remote::DevelopmentClient;
#[cfg(feature = "system-helper-client")]
pub use remote::{PrivilegedClient, PrivilegedClientError, PrivilegedJobHandle};
pub use service::{InProcessClient, JobHandle, ServiceError};

pub trait EventSink: Send + Sync {
    fn progress(&self, progress: defrag_domain::JobProgress);
    fn map_updated(&self, full_snapshot: bool, bins: Vec<defrag_domain::MapBin>);
    fn defrag_progress(&self, progress: DefragProgress);
    /// Publish physical I/O that has been announced but is not yet reflected
    /// in the drive map. An empty pair clears the pending set.
    fn defrag_pending_io(&self, reading: Vec<PhysicalRange>, writing: Vec<PhysicalRange>);
    fn defrag_file_updated(
        &self,
        file: defrag_domain::FileReport,
        fragmentation: defrag_domain::FragmentationMetrics,
        bytes_moved: u64,
    );
}

pub trait JobControl: Send + Sync {
    fn checkpoint(&self) -> Result<(), ServiceError>;
    fn is_cancelled(&self) -> bool;
}

pub struct PlanExecution {
    pub report: AnalysisReport,
    pub stopped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnalysisAccess {
    MountedReadOnly,
    RawDevice,
}

pub trait FilesystemBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn probe(&self, volume: &Volume) -> SupportStatus;
    fn analysis_access(&self, _volume: &Volume) -> AnalysisAccess {
        AnalysisAccess::MountedReadOnly
    }
    fn analyze(
        &self,
        volume: &Volume,
        job_id: defrag_domain::JobId,
        control: &dyn JobControl,
        events: &dyn EventSink,
    ) -> Result<Box<dyn FilesystemAnalysis>, ServiceError>;
}

pub trait FilesystemAnalysis: Send + Sync {
    fn report(&self) -> &AnalysisReport;
    fn build_plan(&self, policy: &DefragPolicy) -> Result<Box<dyn PreparedPlan>, ServiceError>;
}

pub trait PreparedPlan: Send + Sync {
    fn summary(&self) -> &PlanSummary;
    fn execution_requirements(&self) -> &ExecutionRequirements;
    fn execute(
        &self,
        job_id: defrag_domain::JobId,
        control: &dyn JobControl,
        events: &dyn EventSink,
    ) -> Result<PlanExecution, ServiceError>;
}

pub fn default_backends() -> Vec<Arc<dyn FilesystemBackend>> {
    vec![Arc::new(ext4::Ext4Backend), Arc::new(fat::FatBackend)]
}
