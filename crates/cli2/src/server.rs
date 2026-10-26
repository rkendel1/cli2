//! The local protocol server (`cli2 serve`).
//!
//! A thin transport over [`crate::runtime::Runtime`]. It adds no behaviour of
//! its own: every operation is the runtime's, and so the engine's, including
//! the same policy and approval enforcement as the CLI. There is no endpoint
//! that runs an arbitrary command; the only way to run anything is a plan the
//! runtime made, approved against its exact digest.
//!
//! Transport: a Unix-domain socket, newline-delimited JSON, one request per
//! line answered in order.
//!
//! Access control: the socket lives in a directory only the owner can enter
//! (mode 0700), the socket itself is 0600, and every connection's peer UID is
//! checked against the server's before a single request is read.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cli2_protocol::*;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::controller::Controller;
use crate::runtime::{Runtime, RuntimeError};

/// Longest Unix socket path the platforms accept (macOS: 104 including NUL).
const MAX_SOCKET_PATH: usize = 100;
const MAX_CONNECTIONS: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("{0}")]
    Setup(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct Server {
    runtime: Arc<Runtime>,
    controller: Arc<Controller>,
    listener: UnixListener,
    socket: PathBuf,
    _lock: std::fs::File,
    stop: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
}

/// Stops a running server.
pub struct ServerHandle {
    pub socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ServerHandle {
    /// Stop accepting and remove the socket. Executions keep running to completion.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn check_private_dir(dir: &Path) -> Result<(), ServerError> {
    let md = std::fs::metadata(dir)?;
    // SAFETY: geteuid has no failure modes.
    let me = unsafe { libc::geteuid() };
    if md.uid() != me {
        return Err(ServerError::Setup(format!(
            "{} is not owned by the current user",
            dir.display()
        )));
    }
    if md.mode() & 0o077 != 0 {
        return Err(ServerError::Setup(format!(
            "{} is accessible to other users (mode {:o}); refusing to put the runtime socket there",
            dir.display(),
            md.mode() & 0o777
        )));
    }
    Ok(())
}

impl Server {
    /// Bind the socket. Fails if another runtime already owns this socket.
    pub fn bind(runtime: Arc<Runtime>, socket: &Path) -> Result<Self, ServerError> {
        let controller = Controller::new(runtime.clone());
        Self::bind_with(runtime, controller, socket)
    }

    /// Bind with an existing task controller (to register strategies first).
    pub fn bind_with(
        runtime: Arc<Runtime>,
        controller: Arc<Controller>,
        socket: &Path,
    ) -> Result<Self, ServerError> {
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(ServerError::Setup(format!(
                "the socket path is too long for a Unix socket ({} > {MAX_SOCKET_PATH}): {}; use --socket with a shorter path",
                socket.as_os_str().len(),
                socket.display()
            )));
        }
        let dir = socket
            .parent()
            .ok_or_else(|| ServerError::Setup("the socket path has no directory".into()))?;
        let created = !dir.exists();
        std::fs::create_dir_all(dir)?;
        if created {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        check_private_dir(dir)?;

        // One runtime instance owns the executions and state behind a socket.
        let lock_path = socket.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        // SAFETY: flock on a file descriptor we own.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(ServerError::Setup(format!(
                "another cli2 runtime already owns {}; connect to it instead",
                socket.display()
            )));
        }
        // We hold the lock, so an existing socket file is stale.
        if let Ok(md) = std::fs::symlink_metadata(socket) {
            if md.file_type().is_socket() {
                std::fs::remove_file(socket)?;
            } else {
                return Err(ServerError::Setup(format!(
                    "{} exists and is not a socket",
                    socket.display()
                )));
            }
        }
        let listener = UnixListener::bind(socket)?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            runtime,
            controller,
            listener,
            socket: socket.to_path_buf(),
            _lock: lock,
            stop: Arc::new(AtomicBool::new(false)),
            connections: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn controller(&self) -> Arc<Controller> {
        self.controller.clone()
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// Serve on a background thread.
    pub fn spawn(self) -> ServerHandle {
        let stop = self.stop.clone();
        let socket = self.socket.clone();
        let thread = std::thread::spawn(move || self.serve());
        ServerHandle {
            socket,
            stop,
            thread: Some(thread),
        }
    }

    /// Serve until the stop flag is set.
    pub fn serve(self) {
        while !self.stop.load(Ordering::SeqCst) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let runtime = self.runtime.clone();
                    let controller = self.controller.clone();
                    let stop = self.stop.clone();
                    let counter = self.connections.clone();
                    if counter.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        counter.fetch_sub(1, Ordering::SeqCst);
                        let mut s = stream;
                        let _ = write_line(
                            &mut s,
                            &Response::err(
                                "",
                                ErrorBody::new(
                                    ErrorCode::Busy,
                                    "too many connections; retry later",
                                ),
                            ),
                        );
                        continue;
                    }
                    std::thread::spawn(move || {
                        let _ = handle_connection(runtime, controller, stream, stop);
                        counter.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// UID of the process at the other end of a Unix socket.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let fd = stream.as_raw_fd();
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
        // SAFETY: valid fd and valid out-pointers.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } == 0 {
            return Some(uid);
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: valid fd and a correctly sized out buffer.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut _,
                &mut len,
            )
        };
        if rc == 0 {
            return Some(cred.uid);
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "freebsd", target_os = "linux")))]
    {
        let _ = fd;
        None
    }
}

fn write_line(w: &mut impl Write, r: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(r).expect("response serializes");
    if line.len() > MAX_MESSAGE_BYTES {
        line = serde_json::to_vec(&Response::err(
            r.id.clone(),
            ErrorBody::new(
                ErrorCode::Internal,
                "the response exceeded the maximum message size; request less data",
            ),
        ))
        .expect("serializes");
    }
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

enum Line {
    Message(Vec<u8>),
    TooLong,
    Eof,
    Stop,
}

fn read_line(reader: &mut BufReader<UnixStream>, stop: &AtomicBool) -> std::io::Result<Line> {
    let mut buf = Vec::new();
    loop {
        let mut limited = reader
            .by_ref()
            .take((MAX_MESSAGE_BYTES + 1 - buf.len()) as u64);
        match limited.read_until(b'\n', &mut buf) {
            Ok(0) => return Ok(Line::Eof),
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    return Ok(Line::Message(buf));
                }
                if buf.len() > MAX_MESSAGE_BYTES {
                    return Ok(Line::TooLong);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if stop.load(Ordering::SeqCst) {
                    return Ok(Line::Stop);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

fn handle_connection(
    runtime: Arc<Runtime>,
    controller: Arc<Controller>,
    stream: UnixStream,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut writer = stream.try_clone()?;
    // SAFETY: geteuid has no failure modes.
    let me = unsafe { libc::geteuid() };
    if peer_uid(&stream) != Some(me) {
        let _ = write_line(
            &mut writer,
            &Response::err(
                "",
                ErrorBody::new(
                    ErrorCode::Unauthorized,
                    "this connection is not from an authorized local client",
                ),
            ),
        );
        return Ok(());
    }
    let mut reader = BufReader::new(stream);
    let mut negotiated: Option<u32> = None;
    loop {
        let bytes = match read_line(&mut reader, &stop)? {
            Line::Message(b) => b,
            Line::TooLong => {
                let _ = write_line(
                    &mut writer,
                    &Response::err(
                        "",
                        ErrorBody::new(
                            ErrorCode::InvalidRequest,
                            format!("a request line exceeds {MAX_MESSAGE_BYTES} bytes"),
                        ),
                    ),
                );
                return Ok(());
            }
            Line::Eof | Line::Stop => return Ok(()),
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Request>(&bytes) {
            Err(e) => Response::err(
                "",
                ErrorBody::new(
                    ErrorCode::InvalidRequest,
                    format!("not a valid request: {e}"),
                ),
            ),
            Ok(req) => dispatch(&runtime, &controller, &mut negotiated, req),
        };
        write_line(&mut writer, &response)?;
    }
}

fn params<T: DeserializeOwned>(req: &Request) -> Result<T, ErrorBody> {
    let v = if req.params.is_null() {
        Value::Object(Default::default())
    } else {
        req.params.clone()
    };
    serde_json::from_value(v).map_err(|e| {
        ErrorBody::new(
            ErrorCode::InvalidParams,
            format!("invalid params for {}: {e}", req.op),
        )
    })
}

fn done<T: serde::Serialize>(req: &Request, r: Result<T, RuntimeError>) -> Response {
    match r {
        Ok(v) => Response::ok(req.id.clone(), v),
        Err(e) => Response::err(req.id.clone(), e.body()),
    }
}

/// Negotiate the protocol version: the highest both sides support.
pub fn negotiate(client: &[u32]) -> Option<u32> {
    SUPPORTED_VERSIONS
        .iter()
        .copied()
        .filter(|v| client.contains(v))
        .max()
}

fn dispatch(
    rt: &Runtime,
    ctl: &Arc<Controller>,
    negotiated: &mut Option<u32>,
    req: Request,
) -> Response {
    let fail = |e: ErrorBody| Response::err(req.id.clone(), e);
    if req.op == ops::HELLO {
        let p: HelloParams = match params(&req) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        return match negotiate(&p.versions) {
            Some(v) => {
                *negotiated = Some(v);
                Response::ok(
                    req.id.clone(),
                    HelloResult {
                        protocol_version: v,
                        supported_versions: SUPPORTED_VERSIONS.to_vec(),
                        server: ClientInfo { name: "cli2".into(), version: env!("CARGO_PKG_VERSION").into() },
                        instance_id: rt.instance_id().to_string(),
                        operations: ops::ALL.iter().map(|s| s.to_string()).collect(),
                        limits: Limits { max_message_bytes: MAX_MESSAGE_BYTES, max_event_wait_ms: MAX_EVENT_WAIT_MS },
                    },
                )
            }
            None => fail(
                ErrorBody::new(ErrorCode::UnsupportedProtocolVersion, format!("no common protocol version (server supports {SUPPORTED_VERSIONS:?}, client offered {:?})", p.versions))
                    .with_details(serde_json::json!({"supported_versions": SUPPORTED_VERSIONS})),
            ),
        };
    }
    if !ops::ALL.contains(&req.op.as_str()) {
        return fail(
            ErrorBody::new(
                ErrorCode::UnsupportedOperation,
                format!("unsupported operation {:?}", req.op),
            )
            .with_details(serde_json::json!({"supported_operations": ops::ALL})),
        );
    }
    if negotiated.is_none() {
        return fail(ErrorBody::new(
            ErrorCode::NotInitialized,
            "send `hello` first to negotiate a protocol version",
        ));
    }
    macro_rules! with {
        ($t:ty, |$p:ident| $body:expr) => {{
            let $p: $t = match params(&req) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            done(&req, $body)
        }};
    }
    match req.op.as_str() {
        ops::PROJECT_SCAN => with!(ProjectParams, |p| rt.scan(&p.project)),
        ops::CAPABILITIES_LIST => with!(ProjectParams, |p| rt.capabilities(&p.project)),
        ops::INTENT_PLAN => with!(PlanParams, |p| rt.plan(p)),
        ops::PLAN_GET => with!(PlanIdParams, |p| rt.plan_get(&p.plan_id)),
        ops::PLAN_APPROVE => with!(ApproveParams, |p| rt.approve(&p.plan_id, &p.plan_digest)),
        ops::PLAN_REJECT => with!(RejectParams, |p| rt.reject(&p.plan_id, p.reason.clone())),
        ops::EXECUTION_START => with!(StartParams, |p| match p.approval_id.as_deref() {
            Some(a) => rt.start(&p.plan_id, a),
            None => Err(RuntimeError::new(
                ErrorCode::ApprovalRequired,
                "approval_id is required: approve the plan first with plan.approve"
            )),
        }),
        ops::EXECUTION_GET => with!(ExecutionIdParams, |p| rt.execution(&p.execution_id)),
        ops::EXECUTION_EVENTS => with!(EventsParams, |p| rt.events(&p)),
        ops::EXECUTION_CANCEL => with!(ExecutionIdParams, |p| rt.cancel(&p.execution_id)),
        ops::TASK_LIST => with!(TaskListParams, |p| rt.tasks(p.limit)),
        ops::TASK_GET => with!(TaskGetParams, |p| rt.task(&p.id)),
        ops::EVIDENCE_READ => with!(EvidenceParams, |p| rt.evidence(&p.reference, p.max_bytes)),
        ops::OUTPUT_READ => with!(OutputParams, |p| rt.output(
            &p.output_id,
            &p.part,
            p.max_bytes
        )),
        ops::HISTORY_SEARCH => with!(HistorySearchParams, |p| rt.history_search(&p)),
        ops::WORKFLOW_GET => with!(WorkflowParams, |p| rt.workflow(&p)),
        ops::WORKFLOW_REPLAN => with!(WorkflowParams, |p| rt.workflow_replan(&p)),
        ops::DEVTASK_CREATE => with!(devtask::CreateTaskParams, |p| ctl.create(p)),
        ops::DEVTASK_GET => with!(devtask::TaskIdParams, |p| ctl.get(&p.task_id)),
        ops::DEVTASK_LIST => with!(devtask::TaskListParams, |p| ctl.list(p.limit)),
        ops::DEVTASK_START => with!(devtask::TaskIdParams, |p| ctl.start(&p.task_id)),
        ops::DEVTASK_APPROVE => with!(devtask::ApproveStepParams, |p| ctl.approve(p)),
        ops::DEVTASK_REJECT => with!(devtask::RejectStepParams, |p| ctl.reject(p)),
        ops::DEVTASK_PAUSE => with!(devtask::TaskIdParams, |p| ctl.pause(&p.task_id)),
        ops::DEVTASK_RESUME => with!(devtask::TaskIdParams, |p| ctl.resume(&p.task_id)),
        ops::DEVTASK_CANCEL => with!(devtask::TaskIdParams, |p| ctl.cancel(&p.task_id, None)),
        ops::DEVTASK_EVENTS => with!(devtask::TaskEventsParams, |p| ctl.events(&p)),
        ops::DEVTASK_REPORT => with!(devtask::TaskIdParams, |p| ctl.report(&p.task_id)),
        _ => fail(ErrorBody::new(
            ErrorCode::UnsupportedOperation,
            "unsupported operation",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation_picks_the_highest_common_version() {
        assert_eq!(negotiate(&[1]), Some(1));
        assert_eq!(negotiate(&[3, 2, 1]), Some(1));
        assert_eq!(negotiate(&[2, 3]), None);
        assert_eq!(negotiate(&[]), None);
    }
}
