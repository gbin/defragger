use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct VolumeId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct JobId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct AnalysisId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct PlanId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SupportStatus {
    ReadOnly,
    Defragmentable,
    Unsupported { reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MountState {
    MountedReadWrite,
    MountedReadOnly,
    Unmounted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Volume {
    pub id: VolumeId,
    pub mount_id: Option<u64>,
    pub parent_mount_id: Option<u64>,
    pub device_major: u32,
    pub device_minor: u32,
    pub mount_point: Option<PathBuf>,
    pub source: String,
    pub filesystem: String,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub mount_state: MountState,
    pub read_only: bool,
    pub capacity_bytes: u64,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub support: SupportStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AnalysisCompleteness {
    Complete,
    Partial,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MetadataMix {
    pub filesystem_headers: u16,
    pub journal: u16,
    pub allocation_tables: u16,
    pub file_metadata: u16,
    pub group_descriptors: u16,
    pub block_bitmaps: u16,
    pub file_bitmaps: u16,
    pub reserved: u16,
    pub other: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CategoryMix {
    pub free: u16,
    pub contiguous_data: u16,
    pub fragmented_data: u16,
    pub unscanned_data: u16,
    #[serde(default)]
    pub defrag_staging: u16,
    pub metadata: MetadataMix,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapBin {
    pub offset_bytes: u64,
    pub length_bytes: u64,
    pub mix: CategoryMix,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileReport {
    pub path: PathBuf,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub physical_runs: u32,
    pub minimum_runs: u32,
    pub excess_runs: u32,
    pub average_run_bytes: u64,
    pub eligible_for_plan: bool,
    pub exclusion_reason: Option<String>,
    /// Physical ranges let the UI identify which files occupy a selected
    /// drive-map block.
    #[serde(default)]
    pub physical_ranges: Vec<PhysicalRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PhysicalRange {
    pub offset_bytes: u64,
    pub length_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanCoverage {
    pub files_scanned: u64,
    pub directories_scanned: u64,
    pub skipped_entries: u64,
    pub scanned_allocated_bytes: u64,
    pub total_allocated_data_bytes: u64,
    pub estimated_basis_points: Option<u16>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FragmentationMetrics {
    pub fragmented_files: u64,
    pub fragmented_allocated_bytes: u64,
    pub total_physical_runs: u64,
    pub total_excess_runs: u64,
    pub average_run_bytes: u64,
    pub fragmented_basis_points: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub volume: Volume,
    pub completeness: AnalysisCompleteness,
    pub coverage: ScanCoverage,
    pub fragmentation: FragmentationMetrics,
    pub files: Vec<FileReport>,
    pub map: Vec<MapBin>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum OptimizationMode {
    #[default]
    Defragment,
    Compact,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DefragPolicy {
    #[serde(default)]
    pub mode: OptimizationMode,
    pub minimum_excess_runs: u32,
    pub minimum_file_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RequiredMountState {
    MountedReadWrite,
    MountedReadOnly,
    Unmounted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionRequirements {
    pub mount_state: RequiredMountState,
    pub requires_privilege: bool,
    pub available_in_this_build: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanCandidate {
    pub path: PathBuf,
    pub rewrite_bytes: u64,
    pub current_runs: u32,
    pub target_runs: u32,
    #[serde(default)]
    pub role: PlanCandidateRole,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum PlanCandidateRole {
    #[default]
    FragmentationTarget,
    CompactionSupport,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanSummary {
    pub volume_id: VolumeId,
    pub candidates: Vec<PlanCandidate>,
    pub estimated_rewrite_bytes: u64,
    pub excluded_files: u64,
    pub warnings: Vec<String>,
    pub requirements: ExecutionRequirements,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AnalysisPhase {
    ReadingAllocationMap,
    WalkingFiles,
    BuildingReport,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DefragPhase {
    Revalidating,
    AllocatingDonor,
    MovingExtents,
    EvacuatingClusters,
    VerifyingData,
    CommittingMetadata,
    RefreshingMap,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DefragProgress {
    pub job_id: JobId,
    pub phase: DefragPhase,
    pub files_completed: u64,
    pub files_total: u64,
    pub bytes_moved: u64,
    pub bytes_total: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobProgress {
    pub job_id: JobId,
    pub phase: AnalysisPhase,
    pub files_scanned: u64,
    pub bytes_scanned: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServiceRequest {
    ListVolumes,
    StartAnalysis {
        volume_id: VolumeId,
    },
    Pause {
        job_id: JobId,
    },
    Resume {
        job_id: JobId,
    },
    Cancel {
        job_id: JobId,
    },
    BuildPlan {
        analysis_id: AnalysisId,
        policy: DefragPolicy,
    },
    StartDefrag {
        plan_id: PlanId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServiceEvent {
    Volumes(Vec<Volume>),
    AnalysisStarted {
        job_id: JobId,
    },
    Progress(JobProgress),
    MapUpdated {
        job_id: JobId,
        full_snapshot: bool,
        bins: Vec<MapBin>,
    },
    AnalysisFinished {
        job_id: JobId,
        analysis_id: AnalysisId,
        report: Box<AnalysisReport>,
    },
    PlanFinished {
        plan_id: PlanId,
        summary: PlanSummary,
    },
    DefragStarted {
        job_id: JobId,
        plan_id: PlanId,
    },
    DefragProgress(DefragProgress),
    DefragPendingIo {
        job_id: JobId,
        reading: Vec<PhysicalRange>,
        writing: Vec<PhysicalRange>,
    },
    DefragFileUpdated {
        job_id: JobId,
        file: FileReport,
        fragmentation: FragmentationMetrics,
        bytes_moved: u64,
    },
    DefragFinished {
        job_id: JobId,
        report: Box<AnalysisReport>,
    },
    DefragStopped {
        job_id: JobId,
        report: Box<AnalysisReport>,
    },
    JobPaused {
        job_id: JobId,
    },
    JobResumed {
        job_id: JobId,
    },
    JobCancelled {
        job_id: JobId,
    },
    Failed {
        job_id: Option<JobId>,
        message: String,
    },
}
