//! Evidence: an append-only journal of requests, plans, approvals, process
//! invocations and verification results, plus deterministic postcondition
//! checks.

pub mod journal;
pub mod verify;

pub use journal::{Event, Journal, JournalError, Observation, TaskRecord, TaskSummary};
pub use verify::{verify, StepObservation};
