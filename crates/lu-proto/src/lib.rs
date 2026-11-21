//! Wire types for the controller API: job ids and node names, the job spec, job states, failure
//! classes, node heartbeats, events, size limits and output sanitising.
//!
//! This crate is pure data and rules: no I/O, no async. Everything an agent sends is untrusted,
//! so every field is range-checked, strings are capped, and [`printable`] neutralises anything
//! that could move a cursor or reorder text.

pub mod event;
pub mod failure;
pub mod ids;
pub mod label;
pub mod limits;
pub mod node;
pub mod sanitize;
pub mod spec;
pub mod state;
pub mod view;

pub use event::{AgentEvent, Completion, CompletionError, LogChunk, LogChunkError, Outcome};
pub use failure::{FailureClass, ParseFailureClassError};
pub use ids::{JobId, JobIdError, NodeName, NodeNameError};
pub use label::{valid_label, HARNESS_OK_FALSE, LABEL_SOURCE_PROBED};
pub use limits::{
    ArtifactName, ParseArtifactNameError, DEFAULT_TIMEOUT_SECS, INPUT_LOG_BYTES, JOB_ID_LEN,
    MAX_AFTER_JOBS, MAX_COLLECT_BYTES, MAX_COLLECT_INPUTS, MAX_DIFF_BYTES, MAX_FALLBACKS,
    MAX_GIT_REF_BYTES, MAX_IDEMPOTENCY_KEY_BYTES, MAX_IDLE_TIMEOUT_SECS, MAX_LABEL_BYTES,
    MAX_LABEL_FILES, MAX_LABEL_FILE_BYTES, MAX_LOG_BYTES, MAX_LOG_CHUNK_BYTES, MAX_MODEL_BYTES,
    MAX_NAME_BYTES, MAX_NODE_LABELS, MAX_PROVIDERS, MAX_REIKI_BYTES, MAX_REPORT_BYTES,
    MAX_REPO_BYTES, MAX_REQUIRES_LABELS, MAX_STATS_BYTES, MAX_SUMMARY_BYTES, MAX_TAINTS,
    MAX_TASK_BYTES, MAX_TIMEOUT_SECS, MAX_TOLERATES, MIN_TIMEOUT_SECS,
};
pub use node::{
    valid_provider_name, HarnessState, Heartbeat, HeartbeatError, HostStats, JobStats, NodeStats,
    ParseHarnessStateError, ParseProviderStateError, ProcessStats, ProviderModel, ProviderState,
    ProviderStatus,
};
pub use sanitize::{printable, truncate_chars};
pub use spec::{
    effective_collect, validate_idempotency_key, Choice, CollectMode, CollectModeError, JobEdit,
    JobSpec, JobSpecError, OnConflict, OnConflictError, ANY_PROVIDER, COLLECT_ALIASES,
    COLLECT_MODES, ON_CONFLICT_MODES,
};
pub use state::{CancelEffect, JobState, ParseJobStateError};
pub use view::{JobView, NodeView};
