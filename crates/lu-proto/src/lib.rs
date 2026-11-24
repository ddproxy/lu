//! Wire types for the controller API: job ids and node names, the job spec, job states, failure
//! classes, node heartbeats, events, size limits, build versions and output sanitising.
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
pub mod publish;
pub mod sanitize;
pub mod scope;
pub mod spec;
pub mod state;
pub mod stats;
pub mod version;
pub mod view;

pub use event::{AgentEvent, Completion, CompletionError, LogChunk, LogChunkError, Outcome};
pub use failure::{FailureClass, ParseFailureClassError};
pub use ids::{JobId, JobIdError, NodeName, NodeNameError};
pub use label::{valid_label, HARNESS_OK_FALSE, LABEL_SOURCE_PROBED};
pub use limits::{
    ArtifactName, ParseArtifactNameError, DEFAULT_PRIORITY, DEFAULT_STUCK_AFTER_SECS,
    DEFAULT_TIMEOUT_SECS, INPUT_LOG_BYTES, JOB_ID_LEN, MAX_AFTER_JOBS, MAX_CACHE_DIRS,
    MAX_CACHE_DIR_BYTES, MAX_COLLECT_BYTES, MAX_COLLECT_INPUTS, MAX_CONTEXT_BYTES, MAX_DIFF_BYTES,
    MAX_EXPECT_GLOB_BYTES, MAX_EXPECT_ITEMS, MAX_EXPECT_PATH_BYTES, MAX_FALLBACKS,
    MAX_GIT_REF_BYTES, MAX_GROUP_BYTES, MAX_GROUP_RUNNING, MAX_IDEMPOTENCY_KEY_BYTES,
    MAX_IDLE_TIMEOUT_SECS, MAX_LABEL_BYTES, MAX_LABEL_FILES, MAX_LABEL_FILE_BYTES, MAX_LOG_BYTES,
    MAX_LOG_CHUNK_BYTES, MAX_MODEL_BYTES, MAX_NAME_BYTES, MAX_NODE_LABELS, MAX_ORIGIN_BYTES,
    MAX_ORIGIN_REF_BYTES, MAX_PRIORITY, MAX_PROVIDERS, MAX_REIKI_BYTES, MAX_REPORT_BYTES,
    MAX_REPO_BYTES, MAX_REQUIRES_LABELS, MAX_SPAWN_REPOS, MAX_STATS_BYTES, MAX_STUCK_AFTER_SECS,
    MAX_SUMMARY_BYTES, MAX_TAINTS, MAX_TASK_BYTES, MAX_TIMEOUT_SECS, MAX_TOLERATES, MAX_WORKSPACES,
    MIN_PRIORITY, MIN_STUCK_AFTER_SECS, MIN_TIMEOUT_SECS, UNTRUSTED_CONTEXT_HEADER,
};
pub use node::{
    valid_provider_name, HarnessState, Heartbeat, HeartbeatError, HostStats, JobStats, NodeStats,
    ParseHarnessStateError, ParseProviderStateError, ProcessStats, ProviderModel, ProviderState,
    ProviderStatus, WorkspaceState, DEFAULT_MIN_DISK_FREE_PCT,
};
pub use publish::{PublishGateRecord, PublishRecord};
pub use sanitize::{printable, truncate_chars};
pub use scope::{
    decode_scope_request, decode_socket_line, short_summary, short_summary_with_limit, ScopeAction,
    ScopeArtifact, ScopeChild, ScopeDecodeError, ScopeDependency, ScopeErrorDetail,
    ScopeErrorResponse, ScopePayload, ScopeRequest, ScopeResponse, ScopeResult,
    MAX_SCOPE_REPLY_BYTES, MAX_SCOPE_REQUEST_BYTES, MAX_SCOPE_SUMMARY_BYTES, SCOPE_LIMITS,
    SCOPE_ROUTES, SCOPE_SOURCE,
};
pub use spec::{
    effective_stuck, redact_repo, valid_group, validate_idempotency_key, validate_stack_paths,
    Choice, CollectMode, CollectModeError, ExpectEdit, ExpectMode, ExpectModeError, ExpectSpec,
    GateFailureMode, GateFailureModeError, JobEdit, JobSpec, JobSpecError, OnConflict,
    OnConflictError, PublishSpec, PublishTarget, SourceSpec, StuckPolicy, StuckPolicyError,
    WorkspacePolicy, WorkspacePolicyError, ANY_PROVIDER, COLLECT_ALIASES, COLLECT_MODES,
    DEFAULT_PUBLISH_BRANCH, EXPECT_MODES, GATE_FAILURE_MODES, ON_CONFLICT_MODES, STUCK_POLICIES,
    WORKSPACE_POLICIES,
};
pub use state::{CancelEffect, JobState, ParseJobStateError};
pub use stats::{
    normalize_repository, parse_since, DurationSummary, NodeContribution, ParseStatsGroupError,
    RepoContribution, StatsAggregate, StatsGroup, StatsQuery, StatsReport, STATS_BUCKETS_SECS,
    STATS_BUCKET_LABELS, STATS_GROUPS, TERMINAL_STATES, TOP_REPOSITORIES,
};
pub use version::{same_build, BuildVersion, ParseVersionError};
pub use view::{
    AgentSelfView, CordonView, JobView, NodeHealth, NodeHealthLevel, NodeView,
    WorkspaceCleanResult, WorkspaceRelease, WorkspaceView,
};
