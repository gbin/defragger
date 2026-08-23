use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use defrag_domain::{AnalysisId, DefragPolicy, ServiceEvent, VolumeId};
use defrag_service::{InProcessClient, JobHandle};
use zbus::{Connection, connection::Builder, fdo, interface, message::Header, zvariant::Value};

const BUS_NAME: &str = "io.github.defragger.Helper";
const OBJECT_PATH: &str = "/io/github/defragger/Helper";
const ACTION_READ_ALL: &str = "io.github.defragger.read-all-files";

struct JobEntry {
    owner: String,
    handle: JobHandle,
    last_polled: Instant,
}

struct AnalysisEntry {
    owner: String,
    last_used: Instant,
}

#[derive(Clone, Copy)]
enum AuthorizationMode {
    PolicyKit,
    TrustedPeer,
}

#[derive(Clone)]
struct Helper {
    client: InProcessClient,
    jobs: Arc<Mutex<HashMap<u64, JobEntry>>>,
    analyses: Arc<Mutex<HashMap<u64, AnalysisEntry>>>,
    authorization: AuthorizationMode,
}

impl Default for Helper {
    fn default() -> Self {
        Self {
            client: InProcessClient::new(),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            analyses: Arc::new(Mutex::new(HashMap::new())),
            authorization: AuthorizationMode::PolicyKit,
        }
    }
}

#[interface(name = "io.github.defragger.Helper1")]
impl Helper {
    async fn list_volumes(&self) -> fdo::Result<String> {
        let client = self.client.clone();
        let volumes = blocking::unblock(move || client.list_volumes())
            .await
            .map_err(failed)?;
        serde_json::to_string(&volumes).map_err(failed)
    }

    async fn start_analysis(
        &self,
        volume_id: u64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<u64> {
        let owner = self.authorize(connection, &header).await?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| failed("helper job state was poisoned"))?;
        if jobs.values().any(|entry| entry.owner == owner) {
            return Err(fdo::Error::LimitsExceeded(
                "this client already has an active analysis".into(),
            ));
        }
        let handle = self
            .client
            .start_analysis(VolumeId(volume_id))
            .map_err(failed)?;
        let job_id = handle.id().0;
        jobs.insert(
            job_id,
            JobEntry {
                owner,
                handle,
                last_polled: Instant::now(),
            },
        );
        Ok(job_id)
    }

    async fn next_event(
        &self,
        job_id: u64,
        timeout_ms: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<String> {
        let owner = self.owner(&header)?;
        let jobs = Arc::clone(&self.jobs);
        let analyses = Arc::clone(&self.analyses);
        blocking::unblock(move || {
            let mut jobs = jobs
                .lock()
                .map_err(|_| failed("helper job state was poisoned"))?;
            let entry = owned_job(&mut jobs, job_id, &owner)?;
            entry.last_polled = Instant::now();
            let timeout = Duration::from_millis(u64::from(timeout_ms.min(1_000)));
            let mut event = match entry.handle.events().recv_timeout(timeout) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(String::new()),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    jobs.remove(&job_id);
                    return Err(failed("analysis event stream closed unexpectedly"));
                }
            };
            let terminal = matches!(
                event,
                ServiceEvent::AnalysisFinished { .. }
                    | ServiceEvent::JobCancelled { .. }
                    | ServiceEvent::Failed { .. }
            );
            if let ServiceEvent::AnalysisFinished { analysis_id, .. } = &event {
                analyses
                    .lock()
                    .map_err(|_| failed("helper analysis state was poisoned"))?
                    .insert(
                        analysis_id.0,
                        AnalysisEntry {
                            owner: owner.clone(),
                            last_used: Instant::now(),
                        },
                    );
            }
            // The helper retains the full report for plan construction. The
            // GUI only displays fragmented files, so do not send millions of
            // irrelevant contiguous-file rows through the system bus.
            if let ServiceEvent::AnalysisFinished { report, .. } = &mut event {
                report.files.retain(|file| file.excess_runs > 0);
            }
            let encoded = serde_json::to_string(&event).map_err(failed)?;
            if terminal {
                jobs.remove(&job_id);
            }
            Ok(encoded)
        })
        .await
    }

    fn pause(&self, job_id: u64, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.control_job(job_id, &header, JobHandle::pause)
    }

    fn resume(&self, job_id: u64, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.control_job(job_id, &header, JobHandle::resume)
    }

    fn cancel(&self, job_id: u64, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.control_job(job_id, &header, JobHandle::cancel)
    }

    async fn build_plan(
        &self,
        analysis_id: u64,
        policy: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<(u64, String)> {
        let owner = self.owner(&header)?;
        {
            let mut analyses = self
                .analyses
                .lock()
                .map_err(|_| failed("helper analysis state was poisoned"))?;
            let Some(analysis) = analyses.get_mut(&analysis_id) else {
                return Err(fdo::Error::UnknownObject(format!(
                    "analysis {analysis_id} was not found"
                )));
            };
            if analysis.owner != owner {
                return Err(fdo::Error::AccessDenied(
                    "the analysis belongs to a different client".into(),
                ));
            }
            analysis.last_used = Instant::now();
        }
        let policy: DefragPolicy = serde_json::from_str(&policy).map_err(failed)?;
        let client = self.client.clone();
        let (plan_id, summary) =
            blocking::unblock(move || client.build_plan(AnalysisId(analysis_id), &policy))
                .await
                .map_err(failed)?;
        Ok((plan_id.0, serde_json::to_string(&summary).map_err(failed)?))
    }
}

impl Helper {
    fn trusted_peer() -> Self {
        Self {
            authorization: AuthorizationMode::TrustedPeer,
            ..Self::default()
        }
    }

    fn owner(&self, header: &Header<'_>) -> fdo::Result<String> {
        match self.authorization {
            AuthorizationMode::PolicyKit => sender(header),
            AuthorizationMode::TrustedPeer => Ok("development-peer".into()),
        }
    }

    async fn authorize(&self, connection: &Connection, header: &Header<'_>) -> fdo::Result<String> {
        match self.authorization {
            AuthorizationMode::PolicyKit => authorize_policykit(connection, header).await,
            AuthorizationMode::TrustedPeer => Ok("development-peer".into()),
        }
    }

    fn start_reaper(&self) {
        let client = self.client.clone();
        let jobs = Arc::clone(&self.jobs);
        let analyses = Arc::clone(&self.analyses);
        std::thread::Builder::new()
            .name("defrag-helper-reaper".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(10));
                    let Ok(mut jobs) = jobs.lock() else {
                        break;
                    };
                    jobs.retain(|_, entry| entry.last_polled.elapsed() < Duration::from_secs(30));
                    drop(jobs);
                    let Ok(mut analyses) = analyses.lock() else {
                        break;
                    };
                    let expired = analyses
                        .iter()
                        .filter_map(|(id, entry)| {
                            (entry.last_used.elapsed() >= Duration::from_secs(600)).then_some(*id)
                        })
                        .collect::<Vec<_>>();
                    for analysis_id in expired {
                        analyses.remove(&analysis_id);
                        let _ = client.discard_analysis(AnalysisId(analysis_id));
                    }
                }
            })
            .expect("failed to start helper cleanup thread");
    }

    fn control_job(
        &self,
        job_id: u64,
        header: &Header<'_>,
        operation: fn(&JobHandle),
    ) -> fdo::Result<()> {
        let owner = self.owner(header)?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| failed("helper job state was poisoned"))?;
        let entry = owned_job(&mut jobs, job_id, &owner)?;
        operation(&entry.handle);
        Ok(())
    }
}

fn owned_job<'a>(
    jobs: &'a mut HashMap<u64, JobEntry>,
    job_id: u64,
    owner: &str,
) -> fdo::Result<&'a mut JobEntry> {
    let entry = jobs
        .get_mut(&job_id)
        .ok_or_else(|| fdo::Error::UnknownObject(format!("analysis job {job_id} was not found")))?;
    if entry.owner != owner {
        return Err(fdo::Error::AccessDenied(
            "the analysis job belongs to a different client".into(),
        ));
    }
    Ok(entry)
}

async fn authorize_policykit(connection: &Connection, header: &Header<'_>) -> fdo::Result<String> {
    let owner = sender(header)?;
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await
    .map_err(failed)?;
    let mut subject_details = HashMap::new();
    subject_details.insert("name", Value::from(owner.as_str()));
    let subject = ("system-bus-name", subject_details);
    let mut details = HashMap::new();
    details.insert(
        "polkit.message",
        "Authenticate to inspect all files on the selected disk",
    );
    details.insert("polkit.icon_name", "drive-harddisk");
    let (authorized, _, _): (bool, bool, HashMap<String, String>) = proxy
        .call(
            "CheckAuthorization",
            &(subject, ACTION_READ_ALL, details, 1_u32, ""),
        )
        .await
        .map_err(failed)?;
    if authorized {
        Ok(owner)
    } else {
        Err(fdo::Error::AccessDenied(
            "authorization to inspect all files was not granted".into(),
        ))
    }
}

fn sender(header: &Header<'_>) -> fdo::Result<String> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| fdo::Error::AccessDenied("the D-Bus caller has no unique name".into()))
}

fn failed(error: impl ToString) -> fdo::Error {
    fdo::Error::Failed(error.to_string())
}

pub fn run_system_helper() -> Result<(), Box<dyn std::error::Error>> {
    futures_lite::future::block_on(async {
        let helper = Helper::default();
        helper.start_reaper();
        let _connection = Builder::system()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, helper)?
            .build()
            .await?;
        futures_lite::future::pending::<()>().await;
        #[allow(unreachable_code)]
        Ok::<(), zbus::Error>(())
    })?;
    Ok(())
}

/// Serve the same helper interface on a private D-Bus peer connection.
///
/// The process was already authorized and launched as root by systemd, so a
/// second PolicyKit check would be both redundant and impossible without an
/// installed custom action. Closing the GUI side of the peer connection makes
/// `closed()` return and terminates this transient helper.
pub fn run_development_helper(socket_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(socket_path)?;
    futures_lite::future::block_on(async {
        let helper = Helper::trusted_peer();
        helper.start_reaper();
        let connection = Builder::async_io_unix_stream(stream)
            .server(zbus::Guid::generate())?
            .p2p()
            .serve_at(OBJECT_PATH, helper)?
            .build()
            .await?;
        connection.closed().await;
        Ok::<(), zbus::Error>(())
    })?;
    Ok(())
}
