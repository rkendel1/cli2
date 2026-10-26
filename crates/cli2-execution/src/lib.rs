//! Authority and execution: policy assessment, approval enforcement, and
//! bounded subprocess execution.

pub mod policy;
pub mod process;

pub use policy::{assess, authorize, grant, method_grants, Assessment, AuthorizationError, Policy};
pub use process::{
    run, CancelToken, Observer, ProcessObserver, ProcessOutcome, ProcessSpec, StreamKind,
    DEFAULT_MAX_OUTPUT,
};
