mod block_map;
mod ext4;
mod fat;
mod linux;
mod mounts;
mod service;

use std::sync::Arc;

use defrag_domain::{
    AnalysisReport, DefragPolicy, ExecutionRequirements, PlanSummary, SupportStatus, Volume,
};

pub use service::{InProcessClient, JobHandle, ServiceError};

pub trait EventSink: Send + Sync {
    fn progress(&self, progress: defrag_domain::JobProgress);
    fn map_updated(&self, full_snapshot: bool, bins: Vec<defrag_domain::MapBin>);
}

pub trait JobControl: Send + Sync {
    fn checkpoint(&self) -> Result<(), ServiceError>;
}

pub trait FilesystemBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn probe(&self, volume: &Volume) -> SupportStatus;
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
}

pub fn default_backends() -> Vec<Arc<dyn FilesystemBackend>> {
    vec![Arc::new(ext4::Ext4Backend), Arc::new(fat::FatBackend)]
}
