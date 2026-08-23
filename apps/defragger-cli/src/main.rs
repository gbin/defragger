use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::Duration,
};

use defrag_domain::{
    AnalysisId, AnalysisReport, DefragPolicy, MountState, PhysicalRange, PlanId, ServiceEvent,
    SupportStatus, Volume, VolumeId,
};
#[cfg(all(feature = "development-service", not(feature = "system-helper")))]
use defrag_service::DevelopmentClient;
#[cfg(feature = "system-helper")]
use defrag_service::PrivilegedClient;
#[cfg(any(feature = "development-service", feature = "system-helper"))]
use defrag_service::PrivilegedJobHandle;
use defrag_service::{InProcessClient, JobHandle};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn main() {
    #[cfg(feature = "development-service")]
    if let Some(socket_path) = development_helper_socket() {
        if let Err(error) = defragger_helper::run_development_helper(&socket_path) {
            eprintln!("ERROR development helper: {error}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(error) = run() {
        eprintln!("ERROR {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    let direct = take_flag(&mut arguments, "--direct");
    let command = arguments.first().map(String::as_str).unwrap_or("help");
    if matches!(command, "help" | "-h" | "--help") {
        usage();
        return Ok(());
    }

    let client = Client::connect(direct)?;
    match command {
        "list" if arguments.len() == 1 => list(&client),
        "analyze" if arguments.len() == 2 => {
            let volume_id = resolve_volume_id(&client, &arguments[1])?;
            let (_, report) = analyze(&client, volume_id)?;
            print_report("ANALYSIS", &report);
            Ok(())
        }
        "defrag" => {
            let confirmed = take_flag(&mut arguments, "--yes");
            let require_fully_defragmented =
                take_flag(&mut arguments, "--require-fully-defragmented");
            let legacy_require_zero = take_flag(&mut arguments, "--require-zero-excess");
            if arguments.len() != 2 {
                return Err("defrag expects a device or mount path".into());
            }
            if !confirmed {
                return Err("defrag modifies the filesystem; pass --yes to confirm".into());
            }
            defrag(
                &client,
                resolve_volume_id(&client, &arguments[1])?,
                require_fully_defragmented || legacy_require_zero,
            )
        }
        _ => Err("invalid command; run defragger-cli --help".into()),
    }
}

fn usage() {
    println!(
        "Usage:\n  defragger-cli [--direct] list\n  defragger-cli [--direct] analyze DEVICE_OR_MOUNT\n  defragger-cli [--direct] defrag DEVICE_OR_MOUNT --yes [--require-fully-defragmented]\n\nDEVICE_OR_MOUNT may be a device path, mount point, /dev/disk symlink, or loop backing-image path."
    );
}

fn list(client: &Client) -> Result<(), String> {
    let mut rows = vec![[
        "SOURCE".to_owned(),
        "FS".to_owned(),
        "STATE".to_owned(),
        "MOUNT".to_owned(),
        "OCCUPANCY".to_owned(),
        "SUPPORT".to_owned(),
    ]];
    for volume in client.list_volumes()? {
        let volume_occupancy = occupancy(&volume);
        let volume_state = mount_state(&volume).to_owned();
        rows.push([
            volume.source,
            volume.filesystem,
            volume_state,
            volume
                .mount_point
                .as_deref()
                .map_or_else(|| "-".into(), |path| path.display().to_string()),
            volume_occupancy,
            support(&volume.support).into(),
        ]);
    }
    let mut widths = [0usize; 5];
    for row in &rows {
        for (index, value) in row[..5].iter().enumerate() {
            widths[index] = widths[index].max(value.chars().count());
        }
    }
    for row in rows {
        println!(
            "{:<source_width$}  {:<fs_width$}  {:<state_width$}  {:<mount_width$}  {:<occupancy_width$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            source_width = widths[0],
            fs_width = widths[1],
            state_width = widths[2],
            mount_width = widths[3],
            occupancy_width = widths[4],
        );
    }
    Ok(())
}

fn analyze(client: &Client, volume_id: VolumeId) -> Result<(AnalysisId, AnalysisReport), String> {
    INTERRUPTED.store(false, Ordering::Release);
    install_signal_handler();
    let job = client.start_analysis(volume_id)?;
    loop {
        request_cancel_if_interrupted(&job);
        match job.events().recv_timeout(Duration::from_millis(100)) {
            Ok(ServiceEvent::Progress(progress)) => eprintln!(
                "PROGRESS analysis files={} bytes={} path={}",
                progress.files_scanned,
                progress.bytes_scanned,
                display_path(progress.current_path)
            ),
            Ok(ServiceEvent::AnalysisFinished {
                analysis_id,
                report,
                ..
            }) => return Ok((analysis_id, *report)),
            Ok(ServiceEvent::JobCancelled { .. }) => return Err("cancelled safely".into()),
            Ok(ServiceEvent::Failed { message, .. }) => return Err(message),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("service disconnected before analysis completed".into());
            }
        }
    }
}

fn defrag(client: &Client, volume_id: VolumeId, require_zero: bool) -> Result<(), String> {
    let (analysis_id, before) = analyze(client, volume_id)?;
    print_report("BEFORE", &before);
    let (plan_id, plan) = client.build_plan(analysis_id, &DefragPolicy::default())?;
    println!(
        "PLAN candidates={} bytes={} excluded={}",
        plan.candidates.len(),
        plan.estimated_rewrite_bytes,
        plan.excluded_files
    );
    if !plan.requirements.available_in_this_build {
        return Err("plan cannot be executed by this build".into());
    }

    INTERRUPTED.store(false, Ordering::Release);
    let job = client.start_defrag(plan_id)?;
    let (report, stopped) = loop {
        request_cancel_if_interrupted(&job);
        match job.events().recv_timeout(Duration::from_millis(100)) {
            Ok(ServiceEvent::DefragProgress(progress)) => eprintln!(
                "PROGRESS defrag phase={:?} files={}/{} bytes={}/{} path={}",
                progress.phase,
                progress.files_completed,
                progress.files_total,
                progress.bytes_moved,
                progress.bytes_total,
                display_path(progress.current_path)
            ),
            Ok(ServiceEvent::DefragActivity {
                reading, writing, ..
            }) => eprintln!(
                "ACTIVITY read={} write={}",
                ranges(&reading),
                ranges(&writing)
            ),
            Ok(ServiceEvent::DefragFileUpdated {
                file,
                fragmentation,
                ..
            }) => eprintln!(
                "FILE path={} fragments={} extra_fragments={} remaining_extra_fragments={}",
                file.path.display(),
                file.physical_runs,
                file.excess_runs,
                fragmentation.total_excess_runs
            ),
            Ok(ServiceEvent::DefragFinished { report, .. }) => break (*report, false),
            Ok(ServiceEvent::DefragStopped { report, .. }) => break (*report, true),
            Ok(ServiceEvent::JobCancelled { .. }) => return Err("cancelled safely".into()),
            Ok(ServiceEvent::Failed { message, .. }) => return Err(message),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("service disconnected before defrag completed".into());
            }
        }
    };

    print_report(if stopped { "STOPPED" } else { "AFTER" }, &report);
    if require_zero && report.fragmentation.total_excess_runs != 0 {
        return Err(format!(
            "{} extra fragments remain",
            report.fragmentation.total_excess_runs
        ));
    }
    Ok(())
}

fn print_report(label: &str, report: &AnalysisReport) {
    println!(
        "VOLUME filesystem={} capacity={} used={} free={} occupancy={}",
        report.volume.filesystem,
        format_bytes(Some(report.volume.capacity_bytes)),
        format_bytes(report.volume.used_bytes),
        format_bytes(report.volume.free_bytes),
        occupancy(&report.volume),
    );
    println!(
        "{label} files={} fragmented_files={} fragmented_data={} fragments={} extra_fragments={} skipped={} completeness={:?}",
        report.coverage.files_scanned,
        report.fragmentation.fragmented_files,
        format_basis_points(report.fragmentation.fragmented_basis_points),
        report.fragmentation.total_physical_runs,
        report.fragmentation.total_excess_runs,
        report.coverage.skipped_entries,
        report.completeness
    );
}

enum Client {
    Direct(InProcessClient),
    #[cfg(all(feature = "development-service", not(feature = "system-helper")))]
    Development(DevelopmentClient),
    #[cfg(feature = "system-helper")]
    System(PrivilegedClient),
}

impl Client {
    fn connect(direct: bool) -> Result<Self, String> {
        if direct {
            return Ok(Self::Direct(InProcessClient::new()));
        }
        #[cfg(feature = "system-helper")]
        return PrivilegedClient::connect()
            .map(Self::System)
            .map_err(|error| error.to_string());
        #[cfg(all(feature = "development-service", not(feature = "system-helper")))]
        return DevelopmentClient::connect()
            .map(Self::Development)
            .map_err(|error| error.to_string());
        #[cfg(not(any(feature = "development-service", feature = "system-helper")))]
        Ok(Self::Direct(InProcessClient::new()))
    }

    fn list_volumes(&self) -> Result<Vec<Volume>, String> {
        match self {
            Self::Direct(client) => client.list_volumes().map_err(|error| error.to_string()),
            #[cfg(all(feature = "development-service", not(feature = "system-helper")))]
            Self::Development(client) => client.list_volumes().map_err(|error| error.to_string()),
            #[cfg(feature = "system-helper")]
            Self::System(client) => client.list_volumes().map_err(|error| error.to_string()),
        }
    }

    fn start_analysis(&self, volume_id: VolumeId) -> Result<Job, String> {
        match self {
            Self::Direct(client) => client
                .start_analysis(volume_id)
                .map(Job::Direct)
                .map_err(|error| error.to_string()),
            #[cfg(all(feature = "development-service", not(feature = "system-helper")))]
            Self::Development(client) => client
                .start_analysis(volume_id)
                .map(Job::Privileged)
                .map_err(|error| error.to_string()),
            #[cfg(feature = "system-helper")]
            Self::System(client) => client
                .start_analysis(volume_id)
                .map(Job::Privileged)
                .map_err(|error| error.to_string()),
        }
    }

    fn build_plan(
        &self,
        analysis_id: AnalysisId,
        policy: &DefragPolicy,
    ) -> Result<(PlanId, defrag_domain::PlanSummary), String> {
        match self {
            Self::Direct(client) => client
                .build_plan(analysis_id, policy)
                .map_err(|error| error.to_string()),
            #[cfg(all(feature = "development-service", not(feature = "system-helper")))]
            Self::Development(client) => client
                .build_plan(analysis_id, policy)
                .map_err(|error| error.to_string()),
            #[cfg(feature = "system-helper")]
            Self::System(client) => client
                .build_plan(analysis_id, policy)
                .map_err(|error| error.to_string()),
        }
    }

    fn start_defrag(&self, plan_id: PlanId) -> Result<Job, String> {
        match self {
            Self::Direct(client) => client
                .start_defrag(plan_id)
                .map(Job::Direct)
                .map_err(|error| error.to_string()),
            #[cfg(all(feature = "development-service", not(feature = "system-helper")))]
            Self::Development(client) => client
                .start_defrag(plan_id)
                .map(Job::Privileged)
                .map_err(|error| error.to_string()),
            #[cfg(feature = "system-helper")]
            Self::System(client) => client
                .start_defrag(plan_id)
                .map(Job::Privileged)
                .map_err(|error| error.to_string()),
        }
    }
}

enum Job {
    Direct(JobHandle),
    #[cfg(any(feature = "development-service", feature = "system-helper"))]
    Privileged(PrivilegedJobHandle),
}

impl Job {
    fn events(&self) -> &Receiver<ServiceEvent> {
        match self {
            Self::Direct(handle) => handle.events(),
            #[cfg(any(feature = "development-service", feature = "system-helper"))]
            Self::Privileged(handle) => handle.events(),
        }
    }

    fn cancel(&self) {
        match self {
            Self::Direct(handle) => handle.cancel(),
            #[cfg(any(feature = "development-service", feature = "system-helper"))]
            Self::Privileged(handle) => handle.cancel(),
        }
    }
}

fn take_flag(arguments: &mut Vec<String>, flag: &str) -> bool {
    let found = arguments.iter().position(|argument| argument == flag);
    if let Some(index) = found {
        arguments.remove(index);
        true
    } else {
        false
    }
}

fn parse_volume_id(value: &str) -> Result<VolumeId, String> {
    if let Some((major, minor)) = value.split_once(':') {
        let major = major
            .parse::<u32>()
            .map_err(|_| format!("invalid device major number: {major}"))?;
        let minor = minor
            .parse::<u32>()
            .map_err(|_| format!("invalid device minor number: {minor}"))?;
        return Ok(VolumeId((u64::from(major) << 32) | u64::from(minor)));
    }
    value
        .parse::<u64>()
        .map(VolumeId)
        .map_err(|_| format!("invalid volume id: {value}; expected MAJOR:MINOR"))
}

fn resolve_volume_id(client: &Client, selector: &str) -> Result<VolumeId, String> {
    let volumes = client.list_volumes()?;
    let selector_path = Path::new(selector);
    let mut matches = volumes
        .iter()
        .filter(|volume| {
            path_matches(selector_path, Path::new(&volume.source))
                || volume
                    .mount_point
                    .as_deref()
                    .is_some_and(|mount| path_matches(selector_path, mount))
                || loop_backing_file(volume)
                    .as_deref()
                    .is_some_and(|backing| path_matches(selector_path, backing))
        })
        .map(|volume| volume.id)
        .collect::<Vec<_>>();
    matches.sort_by_key(|id| id.0);
    matches.dedup();
    match matches.as_slice() {
        [id] => Ok(*id),
        [] => {
            // Keep old scripts working, but do not expose transport IDs in the UI.
            if let Ok(id) = parse_volume_id(selector)
                && volumes.iter().any(|volume| volume.id == id)
            {
                return Ok(id);
            }
            let sources = volumes
                .iter()
                .map(|volume| volume.source.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "no volume matches {selector}; available devices: {sources}"
            ))
        }
        _ => Err(format!("{selector} matches more than one volume")),
    }
}

fn path_matches(left: &Path, right: &Path) -> bool {
    left == right
        || fs::canonicalize(left)
            .ok()
            .zip(fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn loop_backing_file(volume: &Volume) -> Option<PathBuf> {
    let device = Path::new(&volume.source).file_name()?.to_str()?;
    if !device.starts_with("loop") {
        return None;
    }
    let value = fs::read_to_string(format!("/sys/class/block/{device}/loop/backing_file")).ok()?;
    let value = value
        .trim()
        .strip_suffix(" (deleted)")
        .unwrap_or(value.trim());
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn mount_state(volume: &Volume) -> &'static str {
    match volume.mount_state {
        MountState::MountedReadWrite => "mounted-rw",
        MountState::MountedReadOnly => "mounted-ro",
        MountState::Unmounted => "unmounted",
    }
}

fn support(status: &SupportStatus) -> &str {
    match status {
        SupportStatus::ReadOnly => "analysis-only",
        SupportStatus::Defragmentable => "defragmentable",
        SupportStatus::Unsupported { .. } => "unsupported",
    }
}

fn occupancy(volume: &Volume) -> String {
    match (volume.used_bytes, volume.capacity_bytes) {
        (Some(used), capacity) if capacity > 0 => {
            let basis_points = u128::from(used)
                .saturating_mul(10_000)
                .checked_div(u128::from(capacity))
                .unwrap_or(0)
                .min(10_000) as u16;
            format_basis_points(Some(basis_points))
        }
        _ => "unknown".into(),
    }
}

fn format_basis_points(value: Option<u16>) -> String {
    value.map_or_else(
        || "unknown".into(),
        |value| format!("{}.{:02}%", value / 100, value % 100),
    )
}

fn format_bytes(value: Option<u64>) -> String {
    let Some(bytes) = value else {
        return "unknown".into();
    };
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = bytes as f64;
    let mut unit = 0usize;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{amount:.1}{}", UNITS[unit])
    }
}

fn display_path(path: Option<PathBuf>) -> String {
    path.map_or_else(|| "-".into(), |path| path.display().to_string())
}

fn ranges(values: &[PhysicalRange]) -> String {
    if values.is_empty() {
        return "-".into();
    }
    values
        .iter()
        .map(|range| format!("{}:{}", range.offset_bytes, range.length_bytes))
        .collect::<Vec<_>>()
        .join(",")
}

fn request_cancel_if_interrupted(job: &Job) {
    if INTERRUPTED.swap(false, Ordering::AcqRel) {
        eprintln!("STOP requested; waiting for a safe boundary");
        job.cancel();
    }
}

fn install_signal_handler() {
    unsafe extern "C" fn interrupted(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::Release);
    }
    // SAFETY: the handler only performs a lock-free atomic store.
    unsafe {
        libc::signal(libc::SIGINT, interrupted as *const () as libc::sighandler_t);
        libc::signal(
            libc::SIGTERM,
            interrupted as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(feature = "development-service")]
fn development_helper_socket() -> Option<PathBuf> {
    let mut arguments = env::args_os();
    let _executable = arguments.next()?;
    if arguments.next()?.to_str()? != "--defragger-development-helper" {
        return None;
    }
    let socket_path = PathBuf::from(arguments.next()?);
    arguments.next().is_none().then_some(socket_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_volume_ids_accept_linux_major_minor_notation() {
        let id = parse_volume_id("259:6").unwrap();
        assert_eq!(id, VolumeId((259_u64 << 32) | 6));
    }

    #[test]
    fn legacy_decimal_volume_ids_still_parse() {
        assert_eq!(
            parse_volume_id("1112396529670").unwrap(),
            VolumeId(1112396529670)
        );
    }
}
