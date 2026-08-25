#[cfg(feature = "development-client")]
use std::{
    env, fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use std::{
    io,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

use defrag_domain::{
    AnalysisId, DefragPolicy, PlanId, PlanSummary, ServiceEvent, Volume, VolumeId,
};
use thiserror::Error;
use zbus::{
    blocking::{Connection, Proxy},
    proxy::MethodFlags,
};

pub const BUS_NAME: &str = "net.gootz.defragger.Helper";
pub const OBJECT_PATH: &str = "/net/gootz/defragger/Helper";
pub const INTERFACE: &str = "net.gootz.defragger.Helper1";

#[derive(Debug, Error)]
pub enum PrivilegedClientError {
    #[error("could not contact the privileged analysis service: {0}")]
    Bus(#[from] zbus::Error),
    #[error("the privileged analysis service returned invalid data: {0}")]
    Protocol(#[from] serde_json::Error),
    #[error("could not launch the transient development helper: {0}")]
    Io(#[from] io::Error),
    #[error("systemd refused to launch the transient development helper")]
    LaunchRejected,
    #[error("the transient development helper did not connect")]
    LaunchTimedOut,
}

#[derive(Clone)]
pub struct PrivilegedClient {
    connection: Connection,
}

impl PrivilegedClient {
    pub fn connect() -> Result<Self, PrivilegedClientError> {
        Ok(Self {
            connection: Connection::system()?,
        })
    }

    #[cfg(feature = "development-client")]
    fn connect_peer(stream: std::os::unix::net::UnixStream) -> Result<Self, PrivilegedClientError> {
        let connection = zbus::blocking::connection::Builder::async_io_unix_stream(stream)
            .p2p()
            .build()?;
        Ok(Self { connection })
    }

    pub fn list_volumes(&self) -> Result<Vec<Volume>, PrivilegedClientError> {
        let volumes: String = self.proxy()?.call("ListVolumes", &())?;
        Ok(serde_json::from_str(&volumes)?)
    }

    pub fn unmount_volume(&self, volume_id: VolumeId) -> Result<(), PrivilegedClientError> {
        let _: Option<()> = self.proxy()?.call_with_flags(
            "UnmountVolume",
            MethodFlags::AllowInteractiveAuth.into(),
            &(volume_id.0),
        )?;
        Ok(())
    }

    /// Start a read-all-files analysis. This call deliberately permits an
    /// interactive PolicyKit challenge because it is made directly in
    /// response to the user pressing Analyze.
    pub fn start_analysis(
        &self,
        volume_id: VolumeId,
    ) -> Result<PrivilegedJobHandle, PrivilegedClientError> {
        let proxy = self.proxy()?;
        let job_id: u64 = proxy
            .call_with_flags(
                "StartAnalysis",
                MethodFlags::AllowInteractiveAuth.into(),
                &(volume_id.0),
            )?
            .expect("StartAnalysis expects a reply");
        self.watch_job(job_id)
    }

    pub fn start_defrag(
        &self,
        plan_id: PlanId,
    ) -> Result<PrivilegedJobHandle, PrivilegedClientError> {
        let proxy = self.proxy()?;
        let job_id: u64 = proxy
            .call_with_flags(
                "StartDefrag",
                MethodFlags::AllowInteractiveAuth.into(),
                &(plan_id.0),
            )?
            .expect("StartDefrag expects a reply");
        self.watch_job(job_id)
    }

    fn watch_job(&self, job_id: u64) -> Result<PrivilegedJobHandle, PrivilegedClientError> {
        let (sender, receiver) = mpsc::channel();
        let polling_client = self.clone();
        std::thread::Builder::new()
            .name(format!("defrag-helper-events-{job_id}"))
            .spawn(move || {
                loop {
                    match polling_client.next_event(job_id, Duration::from_millis(100)) {
                        Ok(Some(event)) => {
                            let terminal = matches!(
                                event,
                                ServiceEvent::AnalysisFinished { .. }
                                    | ServiceEvent::DefragFinished { .. }
                                    | ServiceEvent::DefragStopped { .. }
                                    | ServiceEvent::JobCancelled { .. }
                                    | ServiceEvent::Failed { .. }
                            );
                            if sender.send(event).is_err() || terminal {
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = sender.send(ServiceEvent::Failed {
                                job_id: Some(defrag_domain::JobId(job_id)),
                                message: error.to_string(),
                            });
                            break;
                        }
                    }
                }
            })
            .map_err(|error| PrivilegedClientError::Bus(zbus::Error::Failure(error.to_string())))?;

        Ok(PrivilegedJobHandle {
            job_id,
            receiver,
            client: self.clone(),
        })
    }

    pub fn build_plan(
        &self,
        analysis_id: AnalysisId,
        policy: &DefragPolicy,
    ) -> Result<(PlanId, PlanSummary), PrivilegedClientError> {
        let policy = serde_json::to_string(policy)?;
        let (plan_id, summary): (u64, String) =
            self.proxy()?.call("BuildPlan", &(analysis_id.0, policy))?;
        Ok((PlanId(plan_id), serde_json::from_str(&summary)?))
    }

    fn next_event(
        &self,
        job_id: u64,
        timeout: Duration,
    ) -> Result<Option<ServiceEvent>, PrivilegedClientError> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let event: String = self.proxy()?.call("NextEvent", &(job_id, timeout_ms))?;
        if event.is_empty() {
            Ok(None)
        } else {
            Ok(Some(serde_json::from_str(&event)?))
        }
    }

    fn job_control(&self, method: &str, job_id: u64) {
        if let Ok(proxy) = self.proxy() {
            let _: Result<(), _> = proxy.call(method, &(job_id));
        }
    }

    fn proxy(&self) -> Result<Proxy<'_>, zbus::Error> {
        Proxy::new(&self.connection, BUS_NAME, OBJECT_PATH, INTERFACE)
    }
}

/// Development client backed by the production helper interface on a private
/// D-Bus peer connection. systemd starts the current client executable as a
/// transient root service, so no helper binary or bus policy must be installed.
#[derive(Clone)]
#[cfg(feature = "development-client")]
pub struct DevelopmentClient {
    inner: PrivilegedClient,
}

#[cfg(feature = "development-client")]
impl DevelopmentClient {
    #[allow(
        clippy::disallowed_methods,
        reason = "development mode launches this application as a transient systemd service; it does not launch a filesystem utility"
    )]
    pub fn connect() -> Result<Self, PrivilegedClientError> {
        let socket_path = development_socket_path()?;
        let listener = UnixListener::bind(&socket_path)?;
        let _socket_guard = SocketGuard(&socket_path);
        // The helper runs as root with CAP_DAC_READ_SEARCH but deliberately
        // without CAP_DAC_OVERRIDE. It therefore needs the Unix socket's
        // "other" write bit in order to connect. XDG_RUNTIME_DIR is private
        // to this user, so no other unprivileged account can reach the socket.
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o602))?;
        listener.set_nonblocking(true)?;

        let executable = env::current_exe()?;
        let unit = format!("defragger-development-helper-{}", std::process::id());
        let status = Command::new("systemd-run")
            .args([
                "--system",
                "--quiet",
                "--collect",
                "--service-type=exec",
                "--uid=0",
                "--description=Defragger transient development helper",
                "--property=NoNewPrivileges=yes",
                "--property=CapabilityBoundingSet=CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_RAWIO CAP_SYS_ADMIN",
                "--property=RestrictAddressFamilies=AF_UNIX",
                "--property=LockPersonality=yes",
                "--property=MemoryDenyWriteExecute=yes",
                "--property=PrivateTmp=yes",
                "--property=ProtectControlGroups=yes",
                "--property=ProtectKernelModules=yes",
                "--property=ProtectKernelTunables=yes",
                "--property=RestrictRealtime=yes",
            ])
            .arg(format!("--unit={unit}"))
            .arg(executable)
            .arg("--defragger-development-helper")
            .arg(&socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            return Err(PrivilegedClientError::LaunchRejected);
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(PrivilegedClientError::LaunchTimedOut);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        };
        Ok(Self {
            inner: PrivilegedClient::connect_peer(stream)?,
        })
    }

    pub fn list_volumes(&self) -> Result<Vec<Volume>, PrivilegedClientError> {
        self.inner.list_volumes()
    }

    pub fn unmount_volume(&self, volume_id: VolumeId) -> Result<(), PrivilegedClientError> {
        self.inner.unmount_volume(volume_id)
    }

    pub fn start_analysis(
        &self,
        volume_id: VolumeId,
    ) -> Result<PrivilegedJobHandle, PrivilegedClientError> {
        self.inner.start_analysis(volume_id)
    }

    pub fn build_plan(
        &self,
        analysis_id: AnalysisId,
        policy: &DefragPolicy,
    ) -> Result<(PlanId, PlanSummary), PrivilegedClientError> {
        self.inner.build_plan(analysis_id, policy)
    }

    pub fn start_defrag(
        &self,
        plan_id: PlanId,
    ) -> Result<PrivilegedJobHandle, PrivilegedClientError> {
        self.inner.start_defrag(plan_id)
    }
}

#[cfg(feature = "development-client")]
fn development_socket_path() -> io::Result<PathBuf> {
    let directory = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "XDG_RUNTIME_DIR is not set; transient privileged mode requires a desktop session",
            )
        })?;
    let metadata = fs::metadata(&directory)?;
    // SAFETY: geteuid takes no arguments and has no safety preconditions.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || metadata.uid() != effective_uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "XDG_RUNTIME_DIR must be a private directory owned by the current user",
        ));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(directory.join(format!(
        "defragger-development-{}-{nonce}.socket",
        std::process::id()
    )))
}

#[cfg(feature = "development-client")]
struct SocketGuard<'a>(&'a Path);

#[cfg(feature = "development-client")]
impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

pub struct PrivilegedJobHandle {
    job_id: u64,
    receiver: Receiver<ServiceEvent>,
    client: PrivilegedClient,
}

impl PrivilegedJobHandle {
    pub fn events(&self) -> &Receiver<ServiceEvent> {
        &self.receiver
    }

    pub fn pause(&self) {
        self.client.job_control("Pause", self.job_id);
    }

    pub fn resume(&self) {
        self.client.job_control("Resume", self.job_id);
    }

    pub fn cancel(&self) {
        self.client.job_control("Cancel", self.job_id);
    }
}

impl Drop for PrivilegedJobHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(all(test, feature = "development-client"))]
mod tests {
    use std::{os::unix::net::UnixStream, thread};

    use zbus::{connection::Builder, interface};

    use super::*;

    struct EmptyHelper;

    #[interface(name = "net.gootz.defragger.Helper1")]
    impl EmptyHelper {
        fn list_volumes(&self) -> String {
            "[]".into()
        }
    }

    #[test]
    fn private_peer_uses_the_same_helper_proxy_and_closes_with_the_client() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            zbus::block_on(async {
                let connection = Builder::async_io_unix_stream(server_stream)
                    .server(zbus::Guid::generate())?
                    .p2p()
                    .serve_at(OBJECT_PATH, EmptyHelper)?
                    .build()
                    .await?;
                connection.closed().await;
                Ok::<(), zbus::Error>(())
            })
            .unwrap();
        });

        let client = PrivilegedClient::connect_peer(client_stream).unwrap();
        assert!(client.list_volumes().unwrap().is_empty());
        drop(client);
        server.join().unwrap();
    }
}
