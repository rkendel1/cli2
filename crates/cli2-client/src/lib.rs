//! A small, blocking client for the CLI 2.0 local protocol.
//!
//! It depends only on `cli2-protocol` (the wire types) and `cli2-core` (the
//! existing plan/result contracts). It does not link the runtime, does not
//! parse terminal output, and cannot do anything the protocol does not offer:
//! in particular there is no way to run an arbitrary command, only to plan an
//! intent, approve the exact plan that came back, and start that.
//!
//! ```no_run
//! use cli2_client::Client;
//! let mut c = Client::connect("/path/to/cli2.sock", "my-app")?;
//! let plan = c.plan("/path/to/project", "run the project tests")?;
//! if let Some(p) = &plan.outcome.plan {
//!     let approval = c.approve(&p.plan_id, &p.digest)?;       // a decision, nothing runs
//!     let run = c.start(&p.plan_id, &approval.approval.approval_id)?;
//!     let done = c.wait(&run.execution_id, |event| println!("{event:?}"))?;
//!     println!("process: {:?}, task: {:?}, verification: {:?}", done.process, done.task_status, done.verification);
//! }
//! # Ok::<(), cli2_client::ClientError>(())
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use cli2_protocol::*;
use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("I/O error talking to the runtime: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The runtime refused or failed the request; `code` is stable.
    #[error("{}: {}", format!("{:?}", .0.code), .0.message)]
    Server(ErrorBody),
}

impl ClientError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            ClientError::Server(e) => Some(e.code),
            _ => None,
        }
    }
}

pub struct Client {
    socket: PathBuf,
    name: String,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    hello: HelloResult,
}

impl Client {
    /// Connect and negotiate the protocol version.
    pub fn connect(socket: impl AsRef<Path>, client_name: &str) -> Result<Self, ClientError> {
        let socket = socket.as_ref().to_path_buf();
        let stream = UnixStream::connect(&socket)?;
        let mut c = Client {
            socket,
            name: client_name.to_string(),
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            hello: HelloResult {
                protocol_version: 0,
                supported_versions: vec![],
                server: ClientInfo {
                    name: String::new(),
                    version: String::new(),
                },
                instance_id: String::new(),
                operations: vec![],
                limits: Limits {
                    max_message_bytes: MAX_MESSAGE_BYTES,
                    max_event_wait_ms: MAX_EVENT_WAIT_MS,
                },
            },
        };
        c.hello = c.call(
            ops::HELLO,
            &HelloParams {
                client: ClientInfo {
                    name: client_name.into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
                versions: SUPPORTED_VERSIONS.to_vec(),
            },
        )?;
        Ok(c)
    }

    /// Open a fresh connection to the same runtime (e.g. after a disconnect).
    pub fn reconnect(&self) -> Result<Self, ClientError> {
        Client::connect(&self.socket, &self.name)
    }

    pub fn server(&self) -> &HelloResult {
        &self.hello
    }

    /// Send a request and decode the result. Unknown operations are reported
    /// by the server (`unsupported_operation`), not guessed at here.
    pub fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        op: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        let id = format!("c{}", self.next_id);
        self.next_id += 1;
        let req = Request {
            id: id.clone(),
            op: op.into(),
            params: serde_json::to_value(params)
                .map_err(|e| ClientError::Protocol(e.to_string()))?,
        };
        let mut line =
            serde_json::to_vec(&req).map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp)? == 0 {
            return Err(ClientError::Protocol(
                "the runtime closed the connection".into(),
            ));
        }
        let r: Response = serde_json::from_str(&resp)
            .map_err(|e| ClientError::Protocol(format!("unreadable response: {e}")))?;
        if r.id != id && !r.id.is_empty() {
            return Err(ClientError::Protocol(format!(
                "response {} does not answer request {id}",
                r.id
            )));
        }
        match (r.ok, r.result, r.error) {
            (true, Some(v), _) => serde_json::from_value(v)
                .map_err(|e| ClientError::Protocol(format!("unexpected result shape: {e}"))),
            (false, _, Some(e)) => Err(ClientError::Server(e)),
            _ => Err(ClientError::Protocol("malformed response".into())),
        }
    }

    pub fn scan(&mut self, project: &str) -> Result<ScanSummary, ClientError> {
        self.call(
            ops::PROJECT_SCAN,
            &ProjectParams {
                project: project.into(),
            },
        )
    }

    pub fn capabilities(&mut self, project: &str) -> Result<CapabilitiesResult, ClientError> {
        self.call(
            ops::CAPABILITIES_LIST,
            &ProjectParams {
                project: project.into(),
            },
        )
    }

    /// Plan an intent. Never executes.
    pub fn plan(&mut self, project: &str, intent: &str) -> Result<PlanResult, ClientError> {
        self.plan_with(PlanParams {
            project: project.into(),
            intent: intent.into(),
            idempotency_key: None,
            constraints: Default::default(),
            bindings: Default::default(),
            selection: None,
            expectations: vec![],
            capture_help: false,
            resolver: None,
            timeout_secs: None,
        })
    }

    pub fn plan_with(&mut self, p: PlanParams) -> Result<PlanResult, ClientError> {
        self.call(ops::INTENT_PLAN, &p)
    }

    pub fn plan_get(&mut self, plan_id: &str) -> Result<PlanView, ClientError> {
        self.call(
            ops::PLAN_GET,
            &PlanIdParams {
                plan_id: plan_id.into(),
            },
        )
    }

    /// Approve exactly this plan, identified by its ID and full digest.
    pub fn approve(
        &mut self,
        plan_id: &str,
        plan_digest: &str,
    ) -> Result<ApproveResult, ClientError> {
        self.call(
            ops::PLAN_APPROVE,
            &ApproveParams {
                plan_id: plan_id.into(),
                plan_digest: plan_digest.into(),
            },
        )
    }

    pub fn reject(
        &mut self,
        plan_id: &str,
        reason: Option<&str>,
    ) -> Result<RejectResult, ClientError> {
        self.call(
            ops::PLAN_REJECT,
            &RejectParams {
                plan_id: plan_id.into(),
                reason: reason.map(String::from),
            },
        )
    }

    pub fn start(&mut self, plan_id: &str, approval_id: &str) -> Result<StartResult, ClientError> {
        self.call(
            ops::EXECUTION_START,
            &StartParams {
                plan_id: plan_id.into(),
                approval_id: Some(approval_id.into()),
            },
        )
    }

    pub fn execution(&mut self, execution_id: &str) -> Result<ExecutionSnapshot, ClientError> {
        self.call(
            ops::EXECUTION_GET,
            &ExecutionIdParams {
                execution_id: execution_id.into(),
            },
        )
    }

    pub fn events(
        &mut self,
        execution_id: &str,
        after_seq: u64,
        wait_ms: u64,
    ) -> Result<EventsResult, ClientError> {
        self.call(
            ops::EXECUTION_EVENTS,
            &EventsParams {
                execution_id: execution_id.into(),
                after_seq,
                wait_ms,
                limit: None,
            },
        )
    }

    pub fn cancel(&mut self, execution_id: &str) -> Result<CancelResult, ClientError> {
        self.call(
            ops::EXECUTION_CANCEL,
            &ExecutionIdParams {
                execution_id: execution_id.into(),
            },
        )
    }

    /// Follow an execution to its end, handing each event to `on_event`, then
    /// return the final snapshot. Safe to call again after a reconnect: pass
    /// the last sequence you saw to [`Client::wait_from`].
    pub fn wait(
        &mut self,
        execution_id: &str,
        on_event: impl FnMut(&Event),
    ) -> Result<ExecutionSnapshot, ClientError> {
        self.wait_from(execution_id, 0, on_event)
    }

    pub fn wait_from(
        &mut self,
        execution_id: &str,
        mut after_seq: u64,
        mut on_event: impl FnMut(&Event),
    ) -> Result<ExecutionSnapshot, ClientError> {
        loop {
            let batch = self.events(execution_id, after_seq, 2_000)?;
            for e in &batch.events {
                on_event(e);
                after_seq = e.seq;
            }
            if batch.finished {
                return self.execution(execution_id);
            }
        }
    }

    pub fn tasks(&mut self, limit: usize) -> Result<DocumentResult, ClientError> {
        self.call(ops::TASK_LIST, &TaskListParams { limit: Some(limit) })
    }

    pub fn task(&mut self, id: &str) -> Result<DocumentResult, ClientError> {
        self.call(ops::TASK_GET, &TaskGetParams { id: id.into() })
    }

    pub fn evidence(&mut self, reference: &str) -> Result<ContentResult, ClientError> {
        self.call(
            ops::EVIDENCE_READ,
            &EvidenceParams {
                reference: reference.into(),
                max_bytes: None,
            },
        )
    }

    pub fn output(&mut self, output_id: &str, part: &str) -> Result<ContentResult, ClientError> {
        self.call(
            ops::OUTPUT_READ,
            &OutputParams {
                output_id: output_id.into(),
                part: part.into(),
                max_bytes: None,
            },
        )
    }

    /// Read a project file; the returned `sha256` identifies the version a patch applies to.
    pub fn read_file(&mut self, project: &str, path: &str) -> Result<FileReadResult, ClientError> {
        self.call(
            ops::FILE_READ,
            &FileReadParams {
                project: project.into(),
                path: path.into(),
            },
        )
    }

    /// Plan a patch to one file. Nothing is written until the plan is approved by digest and started.
    pub fn plan_edit(&mut self, params: &PlanEditParams) -> Result<PlanResult, ClientError> {
        self.call(ops::PLAN_EDIT, params)
    }

    pub fn history_search(
        &mut self,
        project: &str,
        text: &str,
    ) -> Result<DocumentResult, ClientError> {
        self.call(
            ops::HISTORY_SEARCH,
            &HistorySearchParams {
                project: project.into(),
                text: text.into(),
                limit: None,
                verified_only: false,
            },
        )
    }

    pub fn workflow(
        &mut self,
        project: &str,
        workflow_id: &str,
    ) -> Result<DocumentResult, ClientError> {
        self.call(
            ops::WORKFLOW_GET,
            &WorkflowParams {
                project: project.into(),
                workflow_id: workflow_id.into(),
                bindings: Default::default(),
            },
        )
    }

    /// A fresh plan for a workflow. Never executes, and reuses no old approval.
    pub fn workflow_replan(
        &mut self,
        project: &str,
        workflow_id: &str,
    ) -> Result<DocumentResult, ClientError> {
        self.call(
            ops::WORKFLOW_REPLAN,
            &WorkflowParams {
                project: project.into(),
                workflow_id: workflow_id.into(),
                bindings: Default::default(),
            },
        )
    }

    // ---------------------------------------------------------- development tasks

    pub fn create_task(
        &mut self,
        p: &devtask::CreateTaskParams,
    ) -> Result<devtask::TaskView, ClientError> {
        self.call(ops::DEVTASK_CREATE, p)
    }

    pub fn task_view(&mut self, task_id: &str) -> Result<devtask::TaskView, ClientError> {
        self.call(
            ops::DEVTASK_GET,
            &devtask::TaskIdParams {
                task_id: task_id.into(),
            },
        )
    }

    pub fn task_list(&mut self, limit: usize) -> Result<devtask::TaskListResult, ClientError> {
        self.call(
            ops::DEVTASK_LIST,
            &devtask::TaskListParams { limit: Some(limit) },
        )
    }

    pub fn start_task(&mut self, task_id: &str) -> Result<devtask::TaskView, ClientError> {
        self.call(
            ops::DEVTASK_START,
            &devtask::TaskIdParams {
                task_id: task_id.into(),
            },
        )
    }

    /// Approve the specific operation the task proposed, naming its full digest.
    pub fn approve_step(
        &mut self,
        task_id: &str,
        step_id: &str,
        plan_digest: &str,
    ) -> Result<devtask::TaskView, ClientError> {
        self.call(
            ops::DEVTASK_APPROVE,
            &devtask::ApproveStepParams {
                task_id: task_id.into(),
                step_id: step_id.into(),
                plan_digest: plan_digest.into(),
            },
        )
    }

    pub fn reject_step(
        &mut self,
        task_id: &str,
        step_id: &str,
        reason: Option<&str>,
    ) -> Result<devtask::TaskView, ClientError> {
        self.call(
            ops::DEVTASK_REJECT,
            &devtask::RejectStepParams {
                task_id: task_id.into(),
                step_id: step_id.into(),
                reason: reason.map(String::from),
            },
        )
    }

    pub fn pause_task(&mut self, task_id: &str) -> Result<devtask::TaskView, ClientError> {
        self.call(
            ops::DEVTASK_PAUSE,
            &devtask::TaskIdParams {
                task_id: task_id.into(),
            },
        )
    }

    pub fn resume_task(&mut self, task_id: &str) -> Result<devtask::TaskView, ClientError> {
        self.call(
            ops::DEVTASK_RESUME,
            &devtask::TaskIdParams {
                task_id: task_id.into(),
            },
        )
    }

    pub fn cancel_task(&mut self, task_id: &str) -> Result<devtask::TaskView, ClientError> {
        self.call(
            ops::DEVTASK_CANCEL,
            &devtask::TaskIdParams {
                task_id: task_id.into(),
            },
        )
    }

    pub fn task_events(
        &mut self,
        task_id: &str,
        after_seq: i64,
        wait_ms: u64,
    ) -> Result<devtask::TaskEventsResult, ClientError> {
        self.call(
            ops::DEVTASK_EVENTS,
            &devtask::TaskEventsParams {
                task_id: task_id.into(),
                after_seq,
                wait_ms,
                limit: None,
            },
        )
    }

    pub fn task_report(&mut self, task_id: &str) -> Result<devtask::TaskReport, ClientError> {
        self.call(
            ops::DEVTASK_REPORT,
            &devtask::TaskIdParams {
                task_id: task_id.into(),
            },
        )
    }
}
