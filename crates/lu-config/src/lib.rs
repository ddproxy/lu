//! Lu config. Unknown keys are refused, the file is size-capped, every validation error
//! names its table, and secret files other users can read are refused.

mod error;
mod schedule;
pub mod secret;

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};

use lu_proto::limits::{MAX_PRIORITY, MAX_TIMEOUT_SECS, MIN_PRIORITY, MIN_TIMEOUT_SECS};
use serde::Deserialize;

pub use error::ConfigError;
pub use schedule::ScheduleConfig;
pub use secret::{check_private_file, read_private_file};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Ip(IpAddr),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub https: bool,
    pub host: Host,
    pub port: Option<u16>,
}

/// A config is a few KiB. The cap stops a runaway or hostile file from being read whole.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Largest push token file the controller will read.
pub const MAX_PUBLISH_TOKEN_BYTES: u64 = 8 * 1024;

/// Limits for operator-declared label probes; lu ships no built-in toolchain knowledge.
const MAX_PUBLISH_GATES: usize = 16;
const MAX_PUBLISH_NAME_BYTES: usize = 128;
const MAX_PUBLISH_COMMAND_BYTES: usize = 4096;
const MAX_PUBLISH_PREFIX_BYTES: usize = 128;

const MAX_DETECT_ENTRIES: usize = 16;
pub const MAX_DETECT_ARGV: usize = 8;
pub const MAX_DETECT_TIMEOUT_SECS: u64 = 30;
pub const MIN_DETECT_EVERY_SECS: u64 = 30;

/// Limits for the provider/model pairs an agent offers to provider-agnostic jobs.
pub const MAX_PROVIDER_MODELS: usize = 64;
const MAX_PROVIDER_BYTES: usize = 64;
const MAX_MODEL_BYTES: usize = 128;
const MAX_GROUP_BYTES: usize = 64;
const MAX_GROUP_RUNNING: usize = 64;

/// Operator-defined body markers for classifying a gateway health `503` as a node fault.
pub const MAX_GATEWAY_FAULT_MARKERS: usize = 16;
pub const MAX_GATEWAY_FAULT_MARKER_BYTES: usize = 64;

/// Limits for named harness command profiles. The built-in `opencode` profile counts towards
/// the profile limit; field-level overrides do not create a second profile.
const MAX_HARNESS_PROFILES: usize = 16;
const MAX_HARNESS_ARGV: usize = 64;
const MAX_HARNESS_ARGV_BYTES: usize = 16 * 1024;
const MAX_HARNESS_ARG_BYTES: usize = 4096;
const MAX_HARNESS_FRAGMENT: usize = 16;
const MAX_HARNESS_MESSAGE_BYTES: usize = 4096;
const MAX_HARNESS_ENV: usize = 32;
const MAX_HARNESS_SECRET_ENV: usize = 8;
const MAX_HARNESS_MARKERS: usize = 16;
const MAX_HARNESS_MARKER_BYTES: usize = 64;
const MAX_HARNESS_SERVER_BYTES: usize = 2048;
const MAX_HARNESS_RESULT_FILE_BYTES: usize = 4096;

/// Environment names a harness profile may never copy from the agent process. Controller
/// credentials are supplied outside the harness child and are never injected into it.
const CONTROLLER_CREDENTIAL_ENV: [&str; 4] = ["LU_URL", "LU_TOKEN", "LU_TOKEN_FILE", "LU_CA_FILE"];

const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

const MIN_STUCK_AFTER_SECS: u64 = 60;
const MAX_STUCK_AFTER_SECS: u64 = 86_400;
const DEFAULT_STUCK_AFTER_SECS: u64 = 1_800;

/// Artifact scrubbing. Built-in rule names are reserved for the tags written into stored
/// artifacts, so operator rules must pick another name.
pub const BUILTIN_SCRUB_RULES: [&str; 5] = [
    "private_key",
    "bearer_token",
    "token_prefix",
    "jwt",
    "assignment",
];
pub const MAX_SCRUB_RULES: usize = 16;
const MAX_SCRUB_RULE_NAME_BYTES: usize = 32;
const MAX_SCRUB_PATTERN_BYTES: usize = 512;

/// The agent's default response to a job that stops producing output and using CPU. A job may
/// override this in its spec.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StuckPolicy {
    #[default]
    None,
    Cancel,
    Retry,
}

impl StuckPolicy {
    /// The canonical wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cancel => "cancel",
            Self::Retry => "retry",
        }
    }
}

fn valid_provider_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROVIDER_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The same name rule job specs use for `group`.
fn valid_group_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_GROUP_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub controller: Option<ControllerConfig>,
    pub agent: Option<AgentConfig>,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub log: LogConfig,
    /// Controller-side scrubbing of stored artifacts. Absent means enabled with no extra rules.
    #[serde(default)]
    pub artifacts: Option<ArtifactsConfig>,
    /// Append-only audit record. Omitted or empty disables auditing entirely.
    #[serde(default)]
    pub audit: Option<AuditConfig>,
    /// Job-scoped follow-up submissions. Disabled by default.
    #[serde(default)]
    pub scope: Option<ScopeConfig>,
    /// Node-agent workspace lifecycle. Absent means the documented defaults.
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
    /// Controller-side publishing to review branches. Absent disables publishing.
    #[serde(default)]
    pub publish: Option<PublishConfig>,
    /// Draft retention and inventory limits. Absent means the documented defaults.
    #[serde(default)]
    pub drafts: Option<DraftsConfig>,
    /// Signed webhook intake. Absent disables every intake route.
    #[serde(default)]
    pub intake: Option<IntakeConfig>,
    /// Controller-only credentials for private git mirrors.
    #[serde(default)]
    pub mirror: Option<MirrorConfig>,
    /// Agent-side policies for conflicts in stacked dependency chains.
    #[serde(default)]
    pub stack: Option<StackConfig>,
    /// Controller-side time-aware scheduling. Absent means no calendar gating.
    #[serde(default)]
    pub schedule: Option<ScheduleConfig>,
    /// Calendar definitions. The TOML table is `[[calendar]]`.
    #[serde(default)]
    pub calendar: Vec<lu_schedule::Calendar>,
    /// Calendar attachments. The TOML table is `[[attach]]`.
    #[serde(default)]
    pub attach: Vec<lu_schedule::Attachment>,
    /// The validated schedule built from the fields above and any referenced file.
    #[serde(default, skip)]
    pub schedule_built: Option<lu_schedule::Schedule>,
}

/// `[stack]` — agent-side, fail-closed policies for known-safe conflicted paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StackConfig {
    /// Globs where both sides' added lines are kept, in order, and base deletions are kept.
    pub union_paths: Vec<String>,
    /// Globs where the base side wins, for generated lock files.
    pub base_paths: Vec<String>,
}

impl StackConfig {
    /// Every configured glob, used to check that a job's override only narrows the policy.
    pub fn all_paths(&self) -> impl Iterator<Item = &str> {
        self.union_paths
            .iter()
            .chain(self.base_paths.iter())
            .map(String::as_str)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        lu_proto::validate_stack_paths("[stack] union_paths", &self.union_paths).map_err(|e| {
            ConfigError::Invalid {
                table: "[stack]".to_owned(),
                reason: e.reason().to_owned(),
            }
        })?;
        lu_proto::validate_stack_paths("[stack] base_paths", &self.base_paths).map_err(|e| {
            ConfigError::Invalid {
                table: "[stack]".to_owned(),
                reason: e.reason().to_owned(),
            }
        })?;
        for path in &self.union_paths {
            if self.base_paths.contains(path) {
                return invalid(
                    "[stack]",
                    format!("{path} appears in both union_paths and base_paths"),
                );
            }
        }
        Ok(())
    }
}

pub const DEFAULT_BLOCKED_DEPENDENT_TTL_SECS: u64 = 24 * 60 * 60;

/// `[drafts]` — how long drafts live and how many may be kept.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DraftsConfig {
    /// Retention window; a bare number means seconds. Defaults to `7d`.
    pub ttl: String,
    /// Maximum number of drafts across all origins.
    pub max_drafts: u64,
    /// Maximum number of drafts per origin label.
    pub max_drafts_per_source: u64,
}

impl Default for DraftsConfig {
    fn default() -> Self {
        Self {
            ttl: "7d".to_owned(),
            max_drafts: 10_000,
            max_drafts_per_source: 1_000,
        }
    }
}

/// `[intake]` — signed, generic webhook intake. Off unless present.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeConfig {
    #[serde(default)]
    pub sources: Vec<IntakeSource>,
}

/// `[[intake.sources]]` — one independently signed webhook endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeSource {
    /// Route suffix and provenance label. Must be a safe name.
    pub name: String,
    /// File containing the HMAC-SHA256 key. Checked to be mode 0600 at start.
    pub secret_file: String,
    /// Signature header. Defaults to `X-Hub-Signature-256`.
    #[serde(default = "default_intake_signature_header")]
    pub header: String,
    #[serde(default = "default_intake_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_intake_rate_per_minute")]
    pub rate_per_minute: u64,
    /// Delivery id header used for de-duplication.
    #[serde(default)]
    pub dedupe_header: Option<String>,
    /// How long a delivery id is remembered.
    #[serde(default = "default_intake_dedupe_ttl_secs")]
    pub dedupe_ttl_secs: u64,
    /// Explicit opt-in for unencrypted non-loopback listeners.
    #[serde(default)]
    pub allow_plain_http: bool,
    #[serde(default)]
    pub rules: Vec<IntakeRule>,
}

fn default_intake_signature_header() -> String {
    "X-Hub-Signature-256".to_owned()
}
fn default_intake_max_body_bytes() -> usize {
    1024 * 1024
}
fn default_intake_rate_per_minute() -> u64 {
    60
}
fn default_intake_dedupe_ttl_secs() -> u64 {
    24 * 60 * 60
}

/// A JSON-pointer condition. Exactly one operator must be set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeCondition {
    /// RFC 6901 pointer into the webhook body, e.g. `/actor/login`.
    pub pointer: String,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default, rename = "in")]
    pub in_values: Vec<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub exists: Option<bool>,
}

/// A literal string or a JSON pointer into the webhook body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum IntakeValue {
    Literal(String),
    Pointer { pointer: String },
}

/// `[[intake.sources.rules]]` — maps a signed body to one job.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeRule {
    /// Every condition must match for the rule to apply.
    #[serde(default, rename = "match")]
    pub matches: Vec<IntakeCondition>,
    /// When set, every condition must match or the delivery is dropped and counted.
    #[serde(default)]
    pub allow: Vec<IntakeCondition>,
    pub repo: IntakeValue,
    #[serde(rename = "ref")]
    pub git_ref: IntakeValue,
    #[serde(default)]
    pub base: Option<IntakeValue>,
    /// Operator-authored text. Only pointers listed in `task_vars` may be substituted.
    #[serde(default)]
    pub task_template: String,
    /// Allow-listed pointers for `{{pointer}}` substitution into `task_template`.
    #[serde(default)]
    pub task_vars: Vec<String>,
    /// Pointers copied into the untrusted `context` text.
    #[serde(default)]
    pub context_from: Vec<String>,
    /// Dangerous opt-in: insert payload fields into operator-authored `task`.
    #[serde(default)]
    pub unsafe_inline: bool,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub publish: Option<lu_proto::PublishSpec>,
    /// Template for the job's idempotency key, e.g. `intake-{{/delivery/id}}`.
    #[serde(default)]
    pub key_template: Option<String>,
    #[serde(default)]
    pub auto_approve: bool,
    /// Allow refs whose head repository differs from the target repository.
    #[serde(default)]
    pub allow_forks: bool,
}

/// `[mirror.credentials]` — controller-only token files, never placed in jobs or artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorConfig {
    #[serde(default)]
    pub credentials: Option<MirrorCredentialsConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorCredentialsConfig {
    #[serde(default)]
    pub repos: Vec<MirrorCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorCredential {
    pub name: String,
    pub token_file: String,
}

/// `[scope]` — safe defaults for harness-initiated follow-up jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeConfig {
    /// Master switch. Off by default; no scoped route is reachable until an operator enables it.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_children_per_job")]
    pub max_children_per_job: u64,
    #[serde(default = "default_max_descendants_per_root")]
    pub max_descendants_per_root: u64,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_task_bytes")]
    pub max_task_bytes: usize,
    #[serde(default = "default_max_spawn_per_minute")]
    pub max_spawn_per_minute: u64,
    #[serde(default = "default_max_artifact_read_bytes")]
    pub max_artifact_read_bytes: usize,
    /// Maximum length of a child's summary shown through scope tools. Defaults to 4 KiB.
    #[serde(default = "default_max_summary_bytes")]
    pub max_summary_bytes: usize,
}

impl Default for ScopeConfig {
    fn default() -> Self {
        Self {
            enabled: default_scope_enabled(),
            max_children_per_job: default_max_children_per_job(),
            max_descendants_per_root: default_max_descendants_per_root(),
            max_depth: default_max_depth(),
            max_task_bytes: default_max_task_bytes(),
            max_spawn_per_minute: default_max_spawn_per_minute(),
            max_artifact_read_bytes: default_max_artifact_read_bytes(),
            max_summary_bytes: default_max_summary_bytes(),
        }
    }
}

fn default_scope_enabled() -> bool {
    false
}
fn default_max_children_per_job() -> u64 {
    8
}
fn default_max_descendants_per_root() -> u64 {
    32
}
fn default_max_depth() -> u32 {
    2
}
fn default_max_task_bytes() -> usize {
    64 * 1024
}
fn default_max_spawn_per_minute() -> u64 {
    6
}
fn default_max_artifact_read_bytes() -> usize {
    1024 * 1024
}
fn default_max_summary_bytes() -> usize {
    4096
}

/// `[workspace]` — node-agent workspace retention and disk pressure settings. Durations accept a
/// trailing `s`, `m`, `h` or `d`; a bare number means seconds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkspaceConfig {
    /// Policy for jobs that do not set `workspace` themselves.
    pub default: String,
    /// How long a failed `keep_on_failure` workspace is kept.
    pub failed_ttl: String,
    /// How long a `keep` workspace is kept.
    pub keep_ttl: String,
    /// Optional cap on total workspace bytes; eviction starts above it.
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// Stop evicting once disk use is at or below this percent.
    pub low_water_percent: u8,
    /// Start evicting once disk use is above this percent.
    pub high_water_percent: u8,
    /// How often the agent sweeps TTLs, retries deletions and checks disk pressure.
    pub sweep_interval: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            default: "ephemeral".to_owned(),
            failed_ttl: "6h".to_owned(),
            keep_ttl: "48h".to_owned(),
            max_bytes: None,
            low_water_percent: 65,
            high_water_percent: 80,
            sweep_interval: "60s".to_owned(),
        }
    }
}

impl WorkspaceConfig {
    /// The configured default policy spelling, already validated by [`Config::validate`].
    pub fn default_policy(&self) -> &str {
        self.default.trim()
    }

    /// `failed_ttl` in seconds.
    pub fn failed_ttl_secs(&self) -> u64 {
        parse_duration_secs(&self.failed_ttl).unwrap_or(0)
    }

    /// `keep_ttl` in seconds.
    pub fn keep_ttl_secs(&self) -> u64 {
        parse_duration_secs(&self.keep_ttl).unwrap_or(0)
    }

    /// `sweep_interval` in seconds.
    pub fn sweep_interval_secs(&self) -> u64 {
        parse_duration_secs(&self.sweep_interval).unwrap_or(0)
    }

    /// Validates spellings and the watermark ordering.
    pub fn validate(&self) -> Result<(), String> {
        let valid_policy = matches!(
            self.default_policy(),
            "ephemeral" | "keep_on_failure" | "keep"
        );
        if !valid_policy {
            return Err(format!(
                "[workspace] default must be ephemeral, keep_on_failure or keep (got {:?})",
                self.default
            ));
        }
        for (field, value) in [
            ("failed_ttl", self.failed_ttl.as_str()),
            ("keep_ttl", self.keep_ttl.as_str()),
            ("sweep_interval", self.sweep_interval.as_str()),
        ] {
            let secs = parse_duration_secs(value).ok_or_else(|| {
                format!("[workspace] {field} must be a duration such as 6h, 30m or 60s")
            })?;
            if secs == 0 {
                return Err(format!("[workspace] {field} must be at least 1s"));
            }
        }
        if self.max_bytes == Some(0) {
            return Err("[workspace] max_bytes must be positive when set".to_owned());
        }
        if self.low_water_percent > 100 || self.high_water_percent > 100 {
            return Err("[workspace] water marks must be 0 to 100 percent".to_owned());
        }
        if self.low_water_percent >= self.high_water_percent {
            return Err(
                "[workspace] low_water_percent must be below high_water_percent".to_owned(),
            );
        }
        Ok(())
    }
}

/// Parses `6h`, `30m`, `90s`, `1d` or a plain number of seconds.
/// Parses a config duration (`s`, `m`, `h`, `d` suffixes; bare numbers are seconds).
pub fn parse_duration_secs_public(value: &str) -> Option<u64> {
    parse_duration_secs(value)
}

fn parse_duration_secs(value: &str) -> Option<u64> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    let (digits, multiplier) = match text.as_bytes().last()? {
        b's' | b'S' => (&text[..text.len() - 1], 1),
        b'm' | b'M' => (&text[..text.len() - 1], 60),
        b'h' | b'H' => (&text[..text.len() - 1], 3600),
        b'd' | b'D' => (&text[..text.len() - 1], 86_400),
        _ if text.bytes().all(|b| b.is_ascii_digit()) => (text, 1),
        _ => return None,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(multiplier)
}

/// `[artifacts]` — optional controller-side redaction applied before an artifact is stored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactsConfig {
    /// Turn scrubbing on (the default) or off.
    #[serde(default = "default_scrub")]
    pub scrub: bool,
    /// Scrub `diff.patch` in place. When false (the default), patches are stored unchanged and
    /// inspected for high-confidence secrets in added lines.
    #[serde(default = "default_scrub_patch")]
    pub scrub_patch: bool,
    /// Operator patterns, tried after the built-in rules.
    #[serde(default)]
    pub extra_rules: Vec<ScrubRuleConfig>,
}

fn default_scrub() -> bool {
    true
}

fn default_scrub_patch() -> bool {
    false
}

/// One `[[artifacts.extra_rules]]` entry. `name` becomes the `[scrubbed:<name>]` tag.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrubRuleConfig {
    pub name: String,
    pub pattern: String,
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            scrub: default_scrub(),
            scrub_patch: default_scrub_patch(),
            extra_rules: Vec::new(),
        }
    }
}

/// Configuration for the hash-chained job event record. Off by default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// Absolute path of the JSON Lines file the controller appends to.
    pub path: String,
}

/// `[publish]` — controller-only settings for turning finished jobs into review branches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishConfig {
    /// Forges a job may push to. The name is referenced by `JobSpec.publish.remote`.
    #[serde(default)]
    pub remotes: BTreeMap<String, PublishRemote>,
    /// Named gate commands. `JobSpec.publish.gates` selects from these.
    #[serde(default)]
    pub gates: BTreeMap<String, PublishGate>,
    /// Committer identity written to the generated commit.
    #[serde(default = "default_publish_committer_name")]
    pub committer_name: String,
    #[serde(default = "default_publish_committer_email")]
    pub committer_email: String,
    /// Allow publishing a patch that artifact scrubbing changed. Off by default.
    #[serde(default)]
    pub allow_scrubbed: bool,
    /// Maps `host/owner/name` source patterns to a remote name in `remotes`. An unmatched source
    /// remote has no publish target unless the job's source is itself a configured remote.
    #[serde(default)]
    pub redirects: BTreeMap<String, String>,
    /// Branch names that no source-branch push may update, in addition to the built-in list.
    #[serde(default)]
    pub protected_patterns: Vec<String>,
}

/// One `[publish.remotes.<name>]` entry. The push token is read only from `token_file`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRemote {
    /// HTTPS forge URL. Credentials in the URL are always refused.
    pub url: String,
    /// Absolute path of an owner-only file containing the push token.
    pub token_file: String,
    /// Branch namespaces this remote may receive. Defaults to `["snapshot/"]`.
    #[serde(default = "default_branch_prefixes")]
    pub branch_prefixes: Vec<String>,
    /// Dangerous opt-in: allow a fully approved job to fast-forward its own source branch on this
    /// remote. Never enabled by default.
    #[serde(default)]
    pub allow_source_branch_push: bool,
    /// Maximum successful pushes per hour for this remote.
    pub max_pushes_per_hour: u32,
    /// Test-only loopback `file://` transport for a local bare remote. Requires a `shim.invalid`
    /// HTTPS URL so it can never be confused with a production forge.
    #[serde(default)]
    pub test_file_url: Option<String>,
}

/// One `[publish.gates.<name>]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishGate {
    /// Direct command and arguments, split on whitespace. No shell is involved.
    pub command: String,
    /// Wall-clock limit for the gate job, in seconds.
    pub timeout_secs: u64,
    /// Exit status the gate must report to count as passing.
    pub expected_exit_code: i32,
}

fn default_publish_committer_name() -> String {
    "Lu Publish".to_owned()
}

fn default_publish_committer_email() -> String {
    "lu-publish@localhost".to_owned()
}

fn default_branch_prefixes() -> Vec<String> {
    vec!["snapshot/".to_owned()]
}

/// Redirect patterns and protected branch patterns share one conservative grammar: bounded
/// printable `host/owner/name` text with `*` wildcards and no URL or ref tricks.
fn valid_redirect_pattern(value: &str) -> bool {
    valid_glob(value) && value.contains('/')
}

fn valid_glob(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | '.' | '_' | '-' | '/' | ':'))
        && !value.contains("..")
        && !value.starts_with('/')
        && !value.ends_with('/')
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    pub listen: String,
    pub db: String,
    pub artifacts_dir: String,
    #[serde(default = "default_lease_secs")]
    pub lease_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Maximum number of different nodes a job may try after node-class infrastructure
    /// failures. Bounded also by `max_retries`.
    #[serde(default = "default_node_attempts")]
    pub node_attempts: u32,
    #[serde(default = "default_provider_cooldown_secs")]
    pub provider_cooldown_secs: u64,
    #[serde(default = "default_recovery_grace_secs")]
    pub recovery_grace_secs: u64,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_max_connections_per_addr")]
    pub max_connections_per_addr: usize,
    /// Absolute local repository roots the controller may serve job bases from.
    #[serde(default)]
    pub local_repo_roots: Vec<String>,
    #[serde(default)]
    pub agents: Vec<ControllerAgent>,
    #[serde(default)]
    pub operators: Vec<ControllerOperator>,
    /// Repositories the controller mirrors and pins jobs to.
    #[serde(default)]
    pub repos: Vec<ControllerRepo>,
    /// Concurrency caps for named job groups.
    #[serde(default)]
    pub groups: Vec<ControllerGroup>,
    /// How long dependents of a failed or cancelled job stay blocked before failing.
    #[serde(default = "default_blocked_dependent_ttl")]
    pub blocked_dependent_ttl: String,
    /// Operator GET requests allowed per principal in a 60-second window, across all read routes.
    /// Separate from the login failure throttle; exceeding it returns 429 with Retry-After.
    #[serde(default = "default_read_per_minute")]
    pub read_per_minute: usize,
}

/// Well above a polling UI's roughly 120 requests a minute per operator.
pub const DEFAULT_READ_PER_MINUTE: usize = 1200;

fn default_read_per_minute() -> usize {
    DEFAULT_READ_PER_MINUTE
}

fn default_blocked_dependent_ttl() -> String {
    "24h".to_owned()
}

fn default_lease_secs() -> u64 {
    60
}
fn default_max_retries() -> u32 {
    1
}

/// The default maximum number of nodes tried for one job before a node failure is terminal.
pub const DEFAULT_NODE_ATTEMPTS: u32 = 2;

fn default_node_attempts() -> u32 {
    DEFAULT_NODE_ATTEMPTS
}

fn default_provider_cooldown_secs() -> u64 {
    1800
}
fn default_recovery_grace_secs() -> u64 {
    30
}
fn default_max_connections() -> usize {
    256
}
fn default_max_connections_per_addr() -> usize {
    64
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerAgent {
    pub name: String,
    pub token_sha256: String,
}

/// One named group's concurrency cap. Jobs in unconfigured groups are unlimited.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerGroup {
    pub name: String,
    pub max_running: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ControllerOperator {
    pub name: String,
    pub token_sha256: String,
    /// What the token may do. Defaults to `operator` so existing configurations keep working.
    #[serde(default)]
    pub role: OperatorRole,
    /// Refuses the token from this UTC instant on, written `YYYY-MM-DDTHH:MM:SSZ`.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Refuses job fields that widen what a job may do (spawning children, publishing to the
    /// source branch, accepting scrubbed patches) when this token submits or edits a job.
    #[serde(default)]
    pub restrict_fields: bool,
    /// Permission to read and change scheduling configuration. Default deny.
    #[serde(default)]
    pub schedule: bool,
}

/// An operator token's role. `viewer` is read-only and never sees task text, context, logs or
/// artifacts; `operator` is the full operator API; `admin` is reserved for node-affecting
/// configuration routes and is currently the same as `operator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OperatorRole {
    Viewer,
    #[default]
    Operator,
    Admin,
}

/// Seconds since the Unix epoch for a strict `YYYY-MM-DDTHH:MM:SSZ` UTC timestamp.
pub fn parse_utc_timestamp(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = &text[from..to];
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap => 29,
        _ => 28,
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }
    // Days from civil (proleptic Gregorian), counted from 1970-01-01.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Where the agent obtains a job's base tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BaseSource {
    /// Always clone from the job's `repo`.
    Clone,
    /// Fetch a pack from the controller, failing if it cannot.
    Controller,
    /// Try the controller first and clone when it has no base to serve.
    #[default]
    Auto,
}

/// A repository the controller mirrors itself. Jobs whose `repo` matches `url` are pinned to a
/// commit resolved from this mirror.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerRepo {
    /// An `https` URL, or `file://` when `allow_file_urls` is true.
    pub url: String,
    /// A short label, also used as the mirror directory name.
    pub name: String,
    /// A private file holding `username:password` or a token line.
    #[serde(default)]
    pub credential_file: Option<String>,
    /// Glob allow-list of refs to expose. Defaults to `["*"]`.
    #[serde(default = "default_repo_refs")]
    pub refs: Vec<String>,
    /// Test-only escape hatch for local `file://` repositories.
    #[serde(default)]
    pub allow_file_urls: bool,
}

fn default_repo_refs() -> Vec<String> {
    vec!["*".to_owned()]
}

/// One provider and the model to run for it, in preference order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelConfig {
    pub provider: String,
    pub model: String,
}

/// How a harness receives the job's task text on stdin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessStdin {
    /// The child gets no input, as in earlier releases.
    #[default]
    Null,
    /// Write the task text to the child's stdin, then close the pipe.
    Task,
}

/// One named harness command profile. Fields are optional so an operator can override individual
/// fields of the built-in `opencode` profile. Profiles with any other name must set `argv` and
/// `message`; validation checks the merged result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProfileConfig {
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub wrapper: Option<Vec<String>>,
    #[serde(default)]
    pub model_args: Option<Vec<String>>,
    #[serde(default)]
    pub provider_args: Option<Vec<String>>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub server_args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub secret_env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub stdin: Option<HarnessStdin>,
    #[serde(default)]
    pub result_file: Option<String>,
    #[serde(default)]
    pub provider_markers: Option<Vec<String>>,
    #[serde(default)]
    pub needs_input_markers: Option<Vec<String>>,
}

/// A profile after built-in defaults and operator overrides have been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHarnessProfile {
    pub name: String,
    pub argv: Vec<String>,
    pub message: String,
    pub wrapper: Vec<String>,
    pub model_args: Vec<String>,
    pub provider_args: Vec<String>,
    pub server: Option<String>,
    pub server_args: Vec<String>,
    pub env: Vec<String>,
    pub secret_env: BTreeMap<String, String>,
    pub stdin: HarnessStdin,
    pub result_file: Option<String>,
    pub provider_markers: Vec<String>,
    pub needs_input_markers: Vec<String>,
}

/// The compiled-in profile keeps the command line used before profiles existed. It holds no
/// credential path and no server endpoint; both are operator overrides in the same table.
fn builtin_opencode_profile() -> HarnessProfileConfig {
    HarnessProfileConfig {
        argv: Some(vec![
            "opencode".to_owned(),
            "run".to_owned(),
            "{server_args}".to_owned(),
            "--auto".to_owned(),
            "{model_args}".to_owned(),
            "-f".to_owned(),
            "{task_file}".to_owned(),
            "{message}".to_owned(),
        ]),
        message: Some("Follow the task in the provided file.{context_warning}".to_owned()),
        wrapper: Some(Vec::new()),
        model_args: Some(vec!["-m".to_owned(), "{model}".to_owned()]),
        provider_args: Some(Vec::new()),
        server: None,
        server_args: Some(vec!["--server".to_owned(), "{server}".to_owned()]),
        env: Some(Vec::new()),
        secret_env: Some(BTreeMap::new()),
        stdin: Some(HarnessStdin::Null),
        result_file: None,
        provider_markers: Some(vec![
            "every key in this stream's chain is unavailable".to_owned(),
            "quota".to_owned(),
            "http 429".to_owned(),
            "429 too many requests".to_owned(),
            "rate limit".to_owned(),
        ]),
        needs_input_markers: Some(Vec::new()),
    }
}

fn merge_harness_profile(
    name: &str,
    override_profile: &HarnessProfileConfig,
) -> ResolvedHarnessProfile {
    let builtin = if name == "opencode" {
        builtin_opencode_profile()
    } else {
        HarnessProfileConfig::default()
    };
    let argv = override_profile
        .argv
        .clone()
        .or(builtin.argv)
        .unwrap_or_default();
    let message = override_profile
        .message
        .clone()
        .or(builtin.message)
        .unwrap_or_default();
    let server = override_profile.server.clone().or(builtin.server);
    ResolvedHarnessProfile {
        name: name.to_owned(),
        argv,
        message,
        wrapper: override_profile
            .wrapper
            .clone()
            .or(builtin.wrapper)
            .unwrap_or_default(),
        model_args: override_profile
            .model_args
            .clone()
            .or(builtin.model_args)
            .unwrap_or_default(),
        provider_args: override_profile
            .provider_args
            .clone()
            .or(builtin.provider_args)
            .unwrap_or_default(),
        server,
        server_args: override_profile
            .server_args
            .clone()
            .or(builtin.server_args)
            .unwrap_or_default(),
        env: override_profile
            .env
            .clone()
            .or(builtin.env)
            .unwrap_or_default(),
        secret_env: override_profile
            .secret_env
            .clone()
            .or(builtin.secret_env)
            .unwrap_or_default(),
        stdin: override_profile.stdin.or(builtin.stdin).unwrap_or_default(),
        result_file: override_profile.result_file.clone().or(builtin.result_file),
        provider_markers: override_profile
            .provider_markers
            .clone()
            .or(builtin.provider_markers)
            .unwrap_or_default(),
        needs_input_markers: override_profile
            .needs_input_markers
            .clone()
            .or(builtin.needs_input_markers)
            .unwrap_or_default(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub controller_url: String,
    pub token_file: String,
    pub ca_file: Option<String>,
    pub work_dir: String,
    /// Parse-only compatibility alias. It has no execution meaning: the job spec's `harness`
    /// selects a profile. Kept for one release so mixed-version nodes can share a config file.
    #[serde(default)]
    pub harness: String,
    /// How to obtain a job's base tree. Defaults to `auto`.
    #[serde(default)]
    pub base_source: BaseSource,
    /// Named harness command profiles. The built-in `opencode` profile is always available;
    /// an entry with that name overrides individual fields.
    #[serde(default)]
    pub harness_profiles: BTreeMap<String, HarnessProfileConfig>,
    /// A group (name or number) given write access to each job's working tree, for a harness
    /// that runs as another account, such as a container. Never covers `.git`.
    pub workspace_group: Option<String>,
    /// Numeric uid of the account the harness runs as. The agent chowns each per-job socket and
    /// its private directory to this uid and checks `SO_PEERCRED` against it. Unset means the
    /// harness shares the agent's uid.
    #[serde(default)]
    pub harness_uid: Option<u32>,
    /// Numeric gid for the per-job socket. Defaults to `workspace_group` when set, otherwise to
    /// `harness_uid`. Requires `harness_uid`.
    #[serde(default)]
    pub harness_gid: Option<u32>,
    /// Lets a job with `allow_spawn` get its scope socket while the harness shares the agent's uid
    /// (no `harness_uid`). Such a harness can read everything the agent can, so the socket proves
    /// nothing about who is on the other end; by default the agent refuses the job instead.
    #[serde(default)]
    pub allow_shared_uid_scope: bool,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    /// Default for jobs that do not set `on_stuck`.
    #[serde(default)]
    pub on_stuck: StuckPolicy,
    /// Default continuous stuck interval for jobs that do not set `stuck_after_secs`.
    #[serde(default = "default_stuck_after_secs")]
    pub stuck_after_secs: u64,
    /// Loopback URL of the local provider gateway health endpoint. From 0.8.0 this is the only
    /// accepted spelling.
    pub provider_gateway_health_url: Option<String>,
    /// Private file holding the provider gateway bearer token. From 0.8.0 this is the only
    /// accepted spelling.
    pub provider_gateway_token_file: Option<String>,
    /// Case-insensitive body substrings that classify a `503` from the configured gateway health
    /// endpoint as a node-side gateway fault. Empty by default: without a marker match, a `503`
    /// stays an ordinary failed health read.
    #[serde(default)]
    pub gateway_fault_markers: Vec<String>,
    /// Provider/model pairs this node offers to jobs that request `provider = "any"`.
    #[serde(default)]
    pub provider_models: Vec<ProviderModelConfig>,
    /// Labels this node always advertises. Lu never interprets them.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Taints this node advertises; jobs must tolerate all of them to run here.
    #[serde(default)]
    pub taints: Vec<String>,
    /// Files re-read before each heartbeat; one label per line.
    #[serde(default)]
    pub label_files: Vec<String>,
    /// Commands that publish a label only while they exit successfully.
    #[serde(default)]
    pub detect: Vec<DetectConfig>,
    /// Opt-in removal of leftover build directories between jobs.
    #[serde(default)]
    pub housekeeping: Option<HousekeepingConfig>,
    /// Copied from the top-level `[workspace]` table by the agent binary. Kept out of TOML so a
    /// `[agent] workspace` key stays an unknown-key error.
    #[serde(default, skip)]
    pub workspace: Option<WorkspaceConfig>,
    /// Copied from the top-level `[stack]` table by the agent binary. Kept out of TOML so an
    /// `[agent] stack` key stays an unknown-key error.
    #[serde(default, skip)]
    pub stack: Option<StackConfig>,
}

fn default_heartbeat_secs() -> u64 {
    15
}

fn default_stuck_after_secs() -> u64 {
    DEFAULT_STUCK_AFTER_SECS
}

/// One probe: a command run off the heartbeat path that publishes `label` on exit 0.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectConfig {
    pub label: String,
    /// argv, never a shell string: program plus at most 7 arguments.
    pub command: Vec<String>,
    #[serde(default = "default_detect_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_detect_every_secs")]
    pub every_secs: u64,
}

fn default_detect_timeout_secs() -> u64 {
    5
}

fn default_detect_every_secs() -> u64 {
    300
}

/// Removes old scratch directories between jobs. Every field has the documented default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HousekeepingConfig {
    /// Absolute patterns such as `/srv/work/target-*`. Only the last component may use `*` or `?`.
    pub scratch_globs: Vec<String>,
    /// Extra absolute parents scanned for `target*` children, for build output reached through a
    /// host path bind-mounted from a harness container.
    pub extra_roots: Vec<String>,
    /// Only entries whose newest modification time is at least this old are removed.
    pub min_age_hours: u64,
    /// Act only while the parent filesystem is below this percentage of free space.
    pub min_free_percent: u64,
    /// The most directories removed in one pass.
    pub max_remove_per_pass: usize,
}

impl Default for HousekeepingConfig {
    fn default() -> Self {
        Self {
            scratch_globs: Vec::new(),
            extra_roots: Vec::new(),
            min_age_hours: 6,
            min_free_percent: 15,
            max_remove_per_pass: 10,
        }
    }
}

/// Both paths are required when the table is present.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    pub level: String,
    pub format: LogFormat,
    pub stderr: bool,
    pub file: Option<String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::Text,
            stderr: true,
            file: None,
        }
    }
}

impl Config {
    /// Parses and validates config text.
    ///
    /// The only file this may read is an absolute `schedule.file` reference; all other config
    /// inputs are the text supplied here.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut config = Self::parse_structure(text)?;
        config.schedule_built =
            schedule::build(config.schedule.as_ref(), &config.calendar, &config.attach)?;
        config.validate()?;
        Ok(config)
    }

    /// The fully merged and validated schedule, when scheduling is configured.
    pub fn schedule(&self) -> Option<&lu_schedule::Schedule> {
        self.schedule_built.as_ref()
    }

    /// Minimum free work-filesystem percentage required for dispatch. Zero disables the gate.
    pub fn min_disk_free_pct(&self) -> u8 {
        self.schedule
            .as_ref()
            .and_then(|schedule| schedule.min_disk_free_pct)
            .unwrap_or(lu_proto::DEFAULT_MIN_DISK_FREE_PCT)
    }

    fn parse_structure(text: &str) -> Result<Self, ConfigError> {
        let value: toml::Value =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        schedule::validate_document(&value)?;
        toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.controller.is_none() && self.agent.is_none() {
            return invalid(
                "[config]",
                "at least one of [controller] and [agent] must be present".to_owned(),
            );
        }
        self.validate_log()?;
        self.validate_audit()?;
        self.validate_tls()?;
        self.validate_artifacts()?;
        self.validate_scope()?;
        self.validate_workspace()?;
        self.validate_publish()?;
        self.validate_drafts()?;
        self.validate_intake()?;
        self.validate_mirror()?;
        self.validate_stack()?;
        self.validate_schedule()?;
        if let Some(c) = &self.controller {
            c.validate(self.tls.is_some())?;
        }
        if let Some(a) = &self.agent {
            a.validate()?;
        }
        self.validate_cross_scope()?;
        Ok(())
    }

    fn validate_log(&self) -> Result<(), ConfigError> {
        if !LOG_LEVELS.contains(&self.log.level.as_str()) {
            return invalid(
                "[log]",
                format!(
                    "level must be one of {} (got {:?})",
                    LOG_LEVELS.join(", "),
                    self.log.level
                ),
            );
        }
        if let Some(file) = &self.log.file {
            if file.is_empty() {
                return invalid(
                    "[log]",
                    "file must not be empty; leave it out to log without a file".to_owned(),
                );
            }
            validate_abs_no_dotdot("[log]", "file", file)?;
        }
        Ok(())
    }

    fn validate_audit(&self) -> Result<(), ConfigError> {
        if let Some(audit) = &self.audit {
            validate_abs_no_dotdot("[audit]", "path", &audit.path)?;
        }
        // Tokens that exist to be handed to a UI must leave a record of what they did.
        let audit_off = self.audit.as_ref().is_none_or(|a| a.path.is_empty());
        if audit_off {
            if let Some(controller) = &self.controller {
                if let Some(op) = controller
                    .operators
                    .iter()
                    .find(|op| op.role == OperatorRole::Viewer || op.restrict_fields)
                {
                    return invalid(
                        "[audit]",
                        format!(
                            "path is required while operator {} has role viewer or restrict_fields",
                            op.name
                        ),
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_tls(&self) -> Result<(), ConfigError> {
        match &self.tls {
            Some(tls) if tls.cert.is_empty() || tls.key.is_empty() => {
                invalid("[tls]", "cert and key must both be set".to_owned())
            }
            Some(tls) => {
                validate_abs_no_dotdot("[tls]", "cert", &tls.cert)?;
                validate_abs_no_dotdot("[tls]", "key", &tls.key)
            }
            None => Ok(()),
        }
    }

    fn validate_artifacts(&self) -> Result<(), ConfigError> {
        let Some(artifacts) = &self.artifacts else {
            return Ok(());
        };
        if artifacts.extra_rules.len() > MAX_SCRUB_RULES {
            return invalid(
                "[artifacts]",
                format!(
                    "extra_rules must name at most {MAX_SCRUB_RULES} patterns (got {})",
                    artifacts.extra_rules.len()
                ),
            );
        }
        let mut seen: Vec<String> = Vec::new();
        for (index, rule) in artifacts.extra_rules.iter().enumerate() {
            let table = format!("[[artifacts.extra_rules]] #{index} ({})", rule.name);
            let name_ok = (1..=MAX_SCRUB_RULE_NAME_BYTES).contains(&rule.name.len())
                && rule
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
            if !name_ok {
                return invalid(
                    &table,
                    format!(
                        "name must be 1 to {MAX_SCRUB_RULE_NAME_BYTES} characters from A-Z a-z 0-9 _ -"
                    ),
                );
            }
            if BUILTIN_SCRUB_RULES
                .iter()
                .any(|builtin| builtin.eq_ignore_ascii_case(&rule.name))
                || seen
                    .iter()
                    .any(|other| other.eq_ignore_ascii_case(&rule.name))
            {
                return invalid(
                    &table,
                    format!("name {:?} is reserved or repeated", rule.name),
                );
            }
            seen.push(rule.name.clone());
            if rule.pattern.is_empty() || rule.pattern.len() > MAX_SCRUB_PATTERN_BYTES {
                return invalid(
                    &table,
                    format!(
                        "pattern must be 1 to {MAX_SCRUB_PATTERN_BYTES} bytes (got {})",
                        rule.pattern.len()
                    ),
                );
            }
            if rule.pattern.bytes().any(|b| b == b'\n' || b == b'\r') {
                return invalid(&table, "pattern must be a single line".to_owned());
            }
        }
        Ok(())
    }

    fn validate_scope(&self) -> Result<(), ConfigError> {
        let Some(scope) = &self.scope else {
            return Ok(());
        };
        if scope.max_children_per_job == 0 {
            return invalid(
                "[scope]",
                "max_children_per_job must be at least 1".to_owned(),
            );
        }
        if scope.max_descendants_per_root < scope.max_children_per_job {
            return invalid(
                "[scope]",
                "max_descendants_per_root must be at least max_children_per_job".to_owned(),
            );
        }
        if !(1..=16).contains(&scope.max_depth) {
            return invalid("[scope]", "max_depth must be 1 to 16".to_owned());
        }
        if !(1024..=256 * 1024).contains(&scope.max_task_bytes) {
            return invalid(
                "[scope]",
                "max_task_bytes must be 1024 to 262144".to_owned(),
            );
        }
        if scope.max_spawn_per_minute == 0 {
            return invalid(
                "[scope]",
                "max_spawn_per_minute must be at least 1".to_owned(),
            );
        }
        if !(4096..=10 * 1024 * 1024).contains(&scope.max_artifact_read_bytes) {
            return invalid(
                "[scope]",
                "max_artifact_read_bytes must be 4096 to 10485760".to_owned(),
            );
        }
        if !(256..=4096).contains(&scope.max_summary_bytes) {
            return invalid(
                "[scope]",
                "max_summary_bytes must be 256 to 4096".to_owned(),
            );
        }
        Ok(())
    }

    fn validate_workspace(&self) -> Result<(), ConfigError> {
        if let Some(workspace) = &self.workspace {
            workspace
                .validate()
                .map_err(|reason| ConfigError::Invalid {
                    table: "[workspace]".to_owned(),
                    reason,
                })?;
        }
        Ok(())
    }

    fn validate_publish(&self) -> Result<(), ConfigError> {
        let Some(publish) = &self.publish else {
            return Ok(());
        };
        if publish.committer_name.is_empty()
            || publish.committer_name.len() > MAX_PUBLISH_NAME_BYTES
            || publish
                .committer_name
                .chars()
                .any(|c| c.is_control() || c == '<' || c == '>')
        {
            return invalid(
                "[publish]",
                "committer_name must be 1 to 128 printable characters without < or >".to_owned(),
            );
        }
        if publish.committer_email.is_empty()
            || publish.committer_email.len() > MAX_PUBLISH_NAME_BYTES * 2
            || publish.committer_email.chars().any(char::is_control)
            || publish.committer_email.contains('<')
            || publish.committer_email.contains('>')
            || !publish.committer_email.contains('@')
        {
            return invalid(
                "[publish]",
                "committer_email must be a printable address containing @".to_owned(),
            );
        }
        if publish.gates.len() > MAX_PUBLISH_GATES {
            return invalid(
                "[publish]",
                format!("at most {MAX_PUBLISH_GATES} gates may be configured"),
            );
        }
        for (name, gate) in &publish.gates {
            validate_name("[publish.gates]", name)?;
            if gate.command.is_empty() || gate.command.len() > MAX_PUBLISH_COMMAND_BYTES {
                return invalid(
                    "[publish.gates]",
                    format!("gate {name} command must be 1 to {MAX_PUBLISH_COMMAND_BYTES} bytes"),
                );
            }
            if gate
                .command
                .chars()
                .any(|c| c.is_control() && c != ' ' && c != '\t')
            {
                return invalid(
                    "[publish.gates]",
                    format!("gate {name} command must not contain control characters"),
                );
            }
            if !(1..=3600).contains(&gate.timeout_secs) {
                return invalid(
                    "[publish.gates]",
                    format!("gate {name} timeout_secs must be 1 to 3600"),
                );
            }
            if !(0..=255).contains(&gate.expected_exit_code) {
                return invalid(
                    "[publish.gates]",
                    format!("gate {name} expected_exit_code must be 0 to 255"),
                );
            }
        }
        for (name, remote) in &publish.remotes {
            validate_name("[publish.remotes]", name)?;
            let parsed = parse_http_url(&remote.url)
                .map_err(|e| map_url_error("[publish.remotes]", "url", e, &remote.url))?;
            if !parsed.https {
                return invalid(
                    "[publish.remotes]",
                    format!("remote {name} url must use https"),
                );
            }
            validate_abs_no_dotdot("[publish.remotes]", "token_file", &remote.token_file)?;
            if remote.branch_prefixes.is_empty() {
                return invalid(
                    "[publish.remotes]",
                    format!("remote {name} branch_prefixes must not be empty"),
                );
            }
            for prefix in &remote.branch_prefixes {
                if prefix.is_empty()
                    || prefix.len() > MAX_PUBLISH_PREFIX_BYTES
                    || !prefix.ends_with('/')
                    || prefix.starts_with('/')
                    || prefix.starts_with("refs/")
                    || prefix.contains("..")
                    || prefix.contains('@')
                    || prefix.chars().any(char::is_control)
                {
                    return invalid(
                        "[publish.remotes]",
                        format!("remote {name} has invalid branch prefix {prefix:?}"),
                    );
                }
            }
            if !(1..=10_000).contains(&remote.max_pushes_per_hour) {
                return invalid(
                    "[publish.remotes]",
                    format!("remote {name} max_pushes_per_hour must be 1 to 10000"),
                );
            }
            if let Some(file_url) = &remote.test_file_url {
                let host = match &parsed.host {
                    Host::Name(host) => host.as_str(),
                    Host::Ip(_) => "",
                };
                if host != "shim.invalid" {
                    return invalid(
                        "[publish.remotes]",
                        format!("remote {name} test_file_url requires a shim.invalid url"),
                    );
                }
                let path = file_url.strip_prefix("file://").unwrap_or_default();
                if !path.starts_with('/')
                    || path.contains('@')
                    || path.chars().any(char::is_control)
                {
                    return invalid(
                        "[publish.remotes]",
                        format!("remote {name} test_file_url must be an absolute file:// path"),
                    );
                }
                validate_abs_no_dotdot("[publish.remotes]", "test_file_url", path)?;
            }
        }
        for (pattern, remote) in &publish.redirects {
            if !valid_redirect_pattern(pattern) {
                return invalid(
                    "[publish.redirects]",
                    format!("invalid source pattern {pattern:?}"),
                );
            }
            if !publish.remotes.contains_key(remote) {
                return invalid(
                    "[publish.redirects]",
                    format!("pattern {pattern:?} names unknown remote {remote:?}"),
                );
            }
        }
        for pattern in &publish.protected_patterns {
            if !valid_glob(pattern) || !pattern.bytes().all(|b| b != b'/') {
                return invalid(
                    "[publish]",
                    format!("invalid protected_patterns entry {pattern:?}"),
                );
            }
        }
        Ok(())
    }

    fn validate_drafts(&self) -> Result<(), ConfigError> {
        let Some(drafts) = &self.drafts else {
            return Ok(());
        };
        let secs = parse_duration_secs(&drafts.ttl).ok_or_else(|| ConfigError::Invalid {
            table: "[drafts]".to_owned(),
            reason: format!("ttl {:?} is not a duration", drafts.ttl),
        })?;
        if secs == 0 || secs > 365 * 24 * 60 * 60 {
            return invalid("[drafts]", "ttl must be between 1s and 365d".to_owned());
        }
        if drafts.max_drafts == 0 || drafts.max_drafts_per_source == 0 {
            return invalid(
                "[drafts]",
                "max_drafts and max_drafts_per_source must be positive".to_owned(),
            );
        }
        if drafts.max_drafts_per_source > drafts.max_drafts {
            return invalid(
                "[drafts]",
                "max_drafts_per_source must not exceed max_drafts".to_owned(),
            );
        }
        Ok(())
    }

    fn validate_intake(&self) -> Result<(), ConfigError> {
        let Some(intake) = &self.intake else {
            return Ok(());
        };
        if intake.sources.is_empty() {
            return invalid(
                "[intake]",
                "at least one [[intake.sources]] is required".to_owned(),
            );
        }
        let controller = self
            .controller
            .as_ref()
            .ok_or_else(|| ConfigError::Invalid {
                table: "[intake]".to_owned(),
                reason: "intake requires a [controller] section".to_owned(),
            })?;
        let has_tls = self.tls.is_some();
        let loopback = is_loopback_listen(&controller.listen);
        let mut names = Vec::new();
        for source in &intake.sources {
            let table = format!("[[intake.sources]] ({})", source.name);
            validate_name(&table, &source.name)?;
            if names.contains(&source.name) {
                return invalid(&table, format!("source name {:?} is repeated", source.name));
            }
            names.push(source.name.clone());
            if source.secret_file.is_empty() {
                return invalid(&table, "secret_file must not be empty".to_owned());
            }
            validate_abs_no_dotdot(&table, "secret_file", &source.secret_file)?;
            if source.header.is_empty()
                || source.header.len() > 128
                || source
                    .header
                    .bytes()
                    .any(|b| b <= b' ' || b == b':' || b == 0x7f)
            {
                return invalid(&table, "header must be a valid HTTP header name".to_owned());
            }
            if source.max_body_bytes == 0 || source.max_body_bytes > 16 * 1024 * 1024 {
                return invalid(
                    &table,
                    "max_body_bytes must be between 1 and 16777216".to_owned(),
                );
            }
            if source.rate_per_minute == 0 || source.rate_per_minute > 100_000 {
                return invalid(
                    &table,
                    "rate_per_minute must be between 1 and 100000".to_owned(),
                );
            }
            if source.dedupe_ttl_secs == 0 || source.dedupe_ttl_secs > 7 * 24 * 60 * 60 {
                return invalid(
                    &table,
                    "dedupe_ttl_secs must be between 1 and 604800".to_owned(),
                );
            }
            if let Some(header) = &source.dedupe_header {
                if header.is_empty()
                    || header.len() > 128
                    || header.bytes().any(|b| b <= b' ' || b == b':' || b == 0x7f)
                {
                    return invalid(
                        &table,
                        "dedupe_header must be a valid HTTP header name".to_owned(),
                    );
                }
            }
            if !has_tls && !loopback && !source.allow_plain_http {
                return invalid(
                    &table,
                    "intake requires TLS or a loopback bind; set allow_plain_http = true only behind a TLS-terminating proxy".to_owned(),
                );
            }
            if source.rules.is_empty() {
                return invalid(
                    &table,
                    "at least one [[intake.sources.rules]] is required".to_owned(),
                );
            }
            for (i, rule) in source.rules.iter().enumerate() {
                let rule_table = format!("{table} rule #{i}");
                for (kind, conditions) in [("match", &rule.matches), ("allow", &rule.allow)] {
                    for (j, condition) in conditions.iter().enumerate() {
                        let where_ = format!("{rule_table} {kind}[{j}]");
                        validate_pointer(&where_, &condition.pointer)?;
                        let ops = condition.equals.is_some() as usize
                            + !condition.in_values.is_empty() as usize
                            + condition.prefix.is_some() as usize
                            + condition.regex.is_some() as usize
                            + condition.exists.is_some() as usize;
                        if ops != 1 {
                            return invalid(
                                &where_,
                                "set exactly one of equals, in, prefix, regex or exists".to_owned(),
                            );
                        }
                        if let Some(regex) = &condition.regex {
                            if regex.is_empty()
                                || regex.len() > 512
                                || regex.chars().any(char::is_control)
                            {
                                return invalid(
                                    &where_,
                                    "regex must be 1 to 512 bytes with no control characters"
                                        .to_owned(),
                                );
                            }
                        }
                    }
                }
                validate_intake_value(&rule_table, "repo", &rule.repo)?;
                validate_intake_value(&rule_table, "ref", &rule.git_ref)?;
                if let Some(base) = &rule.base {
                    validate_intake_value(&rule_table, "base", base)?;
                }
                for pointer in rule.task_vars.iter().chain(rule.context_from.iter()) {
                    validate_pointer(&rule_table, pointer)?;
                }
                let vars: Vec<&str> = rule.task_vars.iter().map(String::as_str).collect();
                for (field, template) in [
                    ("task_template", Some(&rule.task_template)),
                    ("key_template", rule.key_template.as_ref()),
                ] {
                    let Some(template) = template else { continue };
                    if template.len() > 4096 {
                        return invalid(&rule_table, format!("{field} must be at most 4096 bytes"));
                    }
                    for pointer in template_placeholders(template) {
                        validate_pointer(&rule_table, &pointer)?;
                        if pointer.len() > MAX_INTAKE_VAR_BYTES {
                            return invalid(
                                &rule_table,
                                format!(
                                    "{field} pointer is longer than {MAX_INTAKE_VAR_BYTES} bytes"
                                ),
                            );
                        }
                        if !vars.contains(&pointer.as_str()) {
                            return invalid(
                                &rule_table,
                                format!("{field} uses {pointer:?}, which is not in task_vars"),
                            );
                        }
                    }
                }
                if !rule.task_template.is_empty()
                    && !template_placeholders(&rule.task_template).is_empty()
                    && !rule.unsafe_inline
                {
                    return invalid(
                        &rule_table,
                        "task_template substitutions require unsafe_inline = true; use context_from instead".to_owned(),
                    );
                }
                if let Some(priority) = rule.priority {
                    if !(MIN_PRIORITY..=MAX_PRIORITY).contains(&priority) {
                        return invalid(
                            &rule_table,
                            format!("priority must be {MIN_PRIORITY} to {MAX_PRIORITY}"),
                        );
                    }
                }
                if let Some(timeout) = rule.timeout {
                    if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout) {
                        return invalid(
                            &rule_table,
                            format!(
                                "timeout must be {MIN_TIMEOUT_SECS} to {MAX_TIMEOUT_SECS} seconds"
                            ),
                        );
                    }
                }
                if let Some(provider) = &rule.provider {
                    if !valid_name(provider) {
                        return invalid(&rule_table, "provider is not a valid name".to_owned());
                    }
                }
                if let Some(group) = &rule.group {
                    if !valid_group_name(group) {
                        return invalid(&rule_table, "group is not a valid name".to_owned());
                    }
                }
                if rule.publish.is_some() && !rule.unsafe_inline {
                    // Publishing is operator-authored configuration, not payload text, so it is
                    // allowed; this branch documents the deliberate distinction.
                }
            }
        }
        Ok(())
    }

    fn validate_mirror(&self) -> Result<(), ConfigError> {
        let Some(mirror) = &self.mirror else {
            return Ok(());
        };
        let Some(creds) = &mirror.credentials else {
            return Ok(());
        };
        let mut names = Vec::new();
        for (i, cred) in creds.repos.iter().enumerate() {
            let table = format!("[mirror.credentials.repos] #{i} ({})", cred.name);
            validate_mirror_credential_name(&table, &cred.name)?;
            if names.contains(&cred.name) {
                return invalid(
                    &table,
                    format!("credential name {:?} is repeated", cred.name),
                );
            }
            names.push(cred.name.clone());
            if cred.token_file.is_empty() {
                return invalid(&table, "token_file must not be empty".to_owned());
            }
            validate_abs_no_dotdot(&table, "token_file", &cred.token_file)?;
        }
        Ok(())
    }

    fn validate_stack(&self) -> Result<(), ConfigError> {
        if let Some(stack) = &self.stack {
            stack.validate()?;
        }
        Ok(())
    }

    fn validate_schedule(&self) -> Result<(), ConfigError> {
        if let Some(built) = &self.schedule_built {
            built.validate().map_err(schedule::map_schedule_error)?;
        }
        Ok(())
    }

    fn validate_cross_scope(&self) -> Result<(), ConfigError> {
        // every token hash unique across agents AND operators
        let mut seen: Vec<(&str, String)> = Vec::new();
        if let Some(c) = &self.controller {
            for (i, a) in c.agents.iter().enumerate() {
                let table = format!("[[controller.agents]] #{} ({})", i + 1, a.name);
                if let Some((other, _)) = seen.iter().find(|(_, h)| h == &a.token_sha256) {
                    return invalid(
                        &table,
                        format!("token_sha256 is the same as one used in {other}"),
                    );
                }
                seen.push(("controller.agents", a.token_sha256.clone()));
            }
            for (i, o) in c.operators.iter().enumerate() {
                let table = format!("[[controller.operators]] #{} ({})", i + 1, o.name);
                if let Some((other, _)) = seen.iter().find(|(_, h)| h == &o.token_sha256) {
                    return invalid(
                        &table,
                        format!(
                            "token_sha256 is the same as one used in {other}; a token must not work in both scopes"
                        ),
                    );
                }
                // also catch reuse inside operators list (already handled per-list, but keep safe)
                seen.push(("controller.operators", o.token_sha256.clone()));
            }
        }
        // heartbeat_secs * 2 must be less than controller lease_secs when both present
        if let (Some(c), Some(a)) = (&self.controller, &self.agent) {
            if a.heartbeat_secs.saturating_mul(2) >= c.lease_secs {
                return invalid(
                    "[agent]",
                    format!(
                        "heartbeat_secs * 2 ({}) must be less than controller lease_secs ({})",
                        a.heartbeat_secs.saturating_mul(2),
                        c.lease_secs
                    ),
                );
            }
        }
        Ok(())
    }
}

impl ControllerConfig {
    /// How long a blocked dependent waits for its predecessor to be retried, in seconds.
    pub fn blocked_dependent_ttl_secs(&self) -> u64 {
        parse_duration_secs(&self.blocked_dependent_ttl)
            .unwrap_or(DEFAULT_BLOCKED_DEPENDENT_TTL_SECS)
    }

    fn validate(&self, has_tls: bool) -> Result<(), ConfigError> {
        let addr: SocketAddr = self.listen.parse().map_err(|_| ConfigError::Invalid {
            table: "[controller]".to_owned(),
            reason: format!(
                "listen must be a socket address like 127.0.0.1:7600 (got {:?})",
                self.listen
            ),
        })?;
        if !addr.ip().is_loopback() && !has_tls {
            return invalid(
                "[controller]",
                format!(
                    "listen {} is not a loopback address, so [tls] is required",
                    self.listen
                ),
            );
        }
        match parse_duration_secs(&self.blocked_dependent_ttl) {
            Some(secs) if (1..=365 * 24 * 60 * 60).contains(&secs) => {}
            _ => {
                return invalid(
                    "[controller]",
                    "blocked_dependent_ttl must be a duration from 1s to 365d".to_owned(),
                )
            }
        }
        if !(1..=100_000).contains(&self.read_per_minute) {
            return invalid(
                "[controller]",
                format!(
                    "read_per_minute must be 1 to 100000 (got {})",
                    self.read_per_minute
                ),
            );
        }
        validate_abs_no_dotdot("[controller]", "db", &self.db)?;
        validate_abs_no_dotdot("[controller]", "artifacts_dir", &self.artifacts_dir)?;
        for (i, root) in self.local_repo_roots.iter().enumerate() {
            validate_abs_no_dotdot(&format!("[controller] local_repo_roots #{i}"), "path", root)?;
        }
        if !(5..=3600).contains(&self.lease_secs) {
            return invalid(
                "[controller]",
                format!("lease_secs must be 5 to 3600 (got {})", self.lease_secs),
            );
        }
        if !(0..=3).contains(&self.max_retries) {
            return invalid(
                "[controller]",
                format!("max_retries must be 0 to 3 (got {})", self.max_retries),
            );
        }
        if !(1..=4).contains(&self.node_attempts) {
            return invalid(
                "[controller]",
                format!("node_attempts must be 1 to 4 (got {})", self.node_attempts),
            );
        }
        if !(60..=86_400).contains(&self.provider_cooldown_secs) {
            return invalid(
                "[controller]",
                format!(
                    "provider_cooldown_secs must be 60 to 86400 (got {})",
                    self.provider_cooldown_secs
                ),
            );
        }
        if !(0..=600).contains(&self.recovery_grace_secs) {
            return invalid(
                "[controller]",
                format!(
                    "recovery_grace_secs must be 0 to 600 (got {})",
                    self.recovery_grace_secs
                ),
            );
        }
        if self.max_connections == 0 {
            return invalid(
                "[controller]",
                format!(
                    "max_connections must be greater than 0 (got {})",
                    self.max_connections
                ),
            );
        }
        if !(1..=self.max_connections).contains(&self.max_connections_per_addr) {
            return invalid(
                "[controller]",
                format!(
                    "max_connections_per_addr must be 1 to max_connections ({}) (got {})",
                    self.max_connections, self.max_connections_per_addr
                ),
            );
        }
        let mut names = Vec::new();
        let mut hashes = Vec::new();
        for (index, agent) in self.agents.iter().enumerate() {
            let table = format!("[[controller.agents]] #{} ({})", index + 1, agent.name);
            validate_name(&table, &agent.name)?;
            if names.contains(&agent.name) {
                return invalid(&table, "name is used by an earlier agent".to_owned());
            }
            validate_sha256(&table, &agent.token_sha256)?;
            if hashes.contains(&agent.token_sha256) {
                return invalid(
                    &table,
                    "token_sha256 is the same as an earlier agent".to_owned(),
                );
            }
            names.push(agent.name.clone());
            hashes.push(agent.token_sha256.clone());
        }
        let mut onames = Vec::new();
        let mut ohashes = Vec::new();
        for (index, op) in self.operators.iter().enumerate() {
            let table = format!("[[controller.operators]] #{} ({})", index + 1, op.name);
            validate_name(&table, &op.name)?;
            if onames.contains(&op.name) {
                return invalid(&table, "name is used by an earlier operator".to_owned());
            }
            validate_sha256(&table, &op.token_sha256)?;
            if let Some(expiry) = &op.expires_at {
                if parse_utc_timestamp(expiry).is_none() {
                    return invalid(
                        &table,
                        "expires_at must be a UTC time like 2030-01-31T12:00:00Z".to_owned(),
                    );
                }
            }
            if ohashes.contains(&op.token_sha256) {
                return invalid(
                    &table,
                    "token_sha256 is the same as an earlier operator".to_owned(),
                );
            }
            onames.push(op.name.clone());
            ohashes.push(op.token_sha256.clone());
        }
        let mut reponames = Vec::new();
        for (index, repo) in self.repos.iter().enumerate() {
            let table = format!("[[controller.repos]] #{} ({})", index + 1, repo.name);
            validate_name(&table, &repo.name)?;
            if reponames.contains(&repo.name) {
                return invalid(&table, "name is used by an earlier repository".to_owned());
            }
            reponames.push(repo.name.clone());
            validate_repo_url(&table, &repo.url, repo.allow_file_urls)?;
            if let Some(file) = &repo.credential_file {
                if file.is_empty() {
                    return invalid(&table, "credential_file must not be empty".to_owned());
                }
                validate_abs_no_dotdot(&table, "credential_file", file)?;
            }
            if repo.refs.is_empty() {
                return invalid(&table, "refs must not be empty; use [\"*\"]".to_owned());
            }
            for pattern in &repo.refs {
                if pattern.is_empty()
                    || pattern.len() > 256
                    || pattern.chars().any(char::is_control)
                {
                    return invalid(
                        &table,
                        "each refs entry must be 1 to 256 characters without control characters"
                            .to_owned(),
                    );
                }
            }
        }
        let mut groupnames = HashSet::new();
        for (index, group) in self.groups.iter().enumerate() {
            let table = format!("[[controller.groups]] #{} ({})", index + 1, group.name);
            if !valid_group_name(&group.name) {
                return invalid(
                    &table,
                    format!("name must be 1 to {MAX_GROUP_BYTES} bytes from A-Z a-z 0-9 . _ -"),
                );
            }
            if !groupnames.insert(group.name.clone()) {
                return invalid(&table, "name is used by an earlier group".to_owned());
            }
            if !(1..=MAX_GROUP_RUNNING).contains(&group.max_running) {
                return invalid(
                    &table,
                    format!(
                        "max_running must be 1 to {MAX_GROUP_RUNNING} (got {})",
                        group.max_running
                    ),
                );
            }
        }
        Ok(())
    }
}

impl AgentConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_name("[agent]", &self.name)?;
        validate_controller_url(&self.controller_url)?;
        if self.token_file.is_empty() {
            return invalid("[agent]", "token_file must not be empty".to_owned());
        }
        validate_abs_no_dotdot("[agent]", "token_file", &self.token_file)?;
        if let Some(ca) = &self.ca_file {
            if ca.is_empty() {
                return invalid("[agent]", "ca_file must not be empty".to_owned());
            }
            validate_abs_no_dotdot("[agent]", "ca_file", ca)?;
        }
        validate_abs_no_dotdot("[agent]", "work_dir", &self.work_dir)?;
        self.validate_harness_profiles()?;
        if let Some(group) = &self.workspace_group {
            let ok = !group.is_empty()
                && group.len() <= 32
                && group
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
            if !ok {
                return invalid(
                    "[agent]",
                    "workspace_group must be 1 to 32 letters, digits, '_', '-' or '.'".to_owned(),
                );
            }
        }
        if self.harness_gid.is_some() && self.harness_uid.is_none() {
            return invalid("[agent]", "harness_gid requires harness_uid".to_owned());
        }
        if !(2..=300).contains(&self.heartbeat_secs) {
            return invalid(
                "[agent]",
                format!(
                    "heartbeat_secs must be 2 to 300 (got {})",
                    self.heartbeat_secs
                ),
            );
        }
        if !(MIN_STUCK_AFTER_SECS..=MAX_STUCK_AFTER_SECS).contains(&self.stuck_after_secs) {
            return invalid(
                "[agent]",
                format!(
                    "stuck_after_secs must be {MIN_STUCK_AFTER_SECS} to {MAX_STUCK_AFTER_SECS} (got {})",
                    self.stuck_after_secs
                ),
            );
        }
        if self.provider_models.len() > MAX_PROVIDER_MODELS {
            return invalid(
                "[agent]",
                format!(
                    "provider_models must name at most {MAX_PROVIDER_MODELS} pairs (got {})",
                    self.provider_models.len()
                ),
            );
        }
        let mut providers = HashSet::new();
        for (index, pair) in self.provider_models.iter().enumerate() {
            let table = format!("[agent.provider_models] #{}", index + 1);
            if !valid_provider_name(&pair.provider) {
                return invalid(
                    &table,
                    format!(
                        "provider must be 1 to {MAX_PROVIDER_BYTES} characters from A-Z a-z 0-9 . _ - (got {:?})",
                        pair.provider
                    ),
                );
            }
            if pair.model.is_empty() || pair.model.len() > MAX_MODEL_BYTES {
                return invalid(
                    &table,
                    format!(
                        "model must be 1 to {MAX_MODEL_BYTES} bytes (got {})",
                        pair.model.len()
                    ),
                );
            }
            if !providers.insert(pair.provider.as_str()) {
                return invalid(
                    &table,
                    format!("provider {:?} duplicates an earlier pair", pair.provider),
                );
            }
        }
        if self.labels.len() > 32 {
            return invalid(
                "[agent]",
                format!(
                    "labels must name at most 32 labels (got {})",
                    self.labels.len()
                ),
            );
        }
        for label in &self.labels {
            if !valid_label(label) {
                return invalid(
                    "[agent]",
                    format!("invalid label {label:?}; it must be 1 to 64 characters from A-Z a-z 0-9 . _ - / with at most one '='"),
                );
            }
        }
        if self.taints.len() > 8 {
            return invalid(
                "[agent]",
                format!(
                    "taints must name at most 8 taints (got {})",
                    self.taints.len()
                ),
            );
        }
        for taint in &self.taints {
            if !valid_label(taint) {
                return invalid(
                    "[agent]",
                    format!("invalid taint {taint:?}; it must be 1 to 64 characters from A-Z a-z 0-9 . _ - / with at most one '='"),
                );
            }
        }
        if self.label_files.len() > 8 {
            return invalid(
                "[agent]",
                format!(
                    "label_files must name at most 8 files (got {})",
                    self.label_files.len()
                ),
            );
        }
        for (index, file) in self.label_files.iter().enumerate() {
            validate_abs_no_dotdot(&format!("[agent] label_files #{index}"), "path", file)?;
        }
        if self.detect.len() > MAX_DETECT_ENTRIES {
            return invalid(
                "[agent]",
                format!(
                    "detect must name at most {MAX_DETECT_ENTRIES} probes (got {})",
                    self.detect.len()
                ),
            );
        }
        let mut seen = HashSet::new();
        for (index, probe) in self.detect.iter().enumerate() {
            let table = format!("[agent.detect] #{}", index + 1);
            if !valid_label(&probe.label) {
                return invalid(
                    &table,
                    format!("invalid label {:?}; it must be 1 to 64 characters from A-Z a-z 0-9 . _ - / with at most one '='", probe.label),
                );
            }
            if !seen.insert(probe.label.as_str()) {
                return invalid(
                    &table,
                    format!("label {:?} duplicates an earlier probe", probe.label),
                );
            }
            if probe.command.is_empty() || probe.command.len() > MAX_DETECT_ARGV {
                return invalid(
                    &table,
                    format!(
                        "command must hold 1 to {MAX_DETECT_ARGV} argv entries (got {})",
                        probe.command.len()
                    ),
                );
            }
            if probe
                .command
                .iter()
                .any(|arg| arg.is_empty() || arg.chars().any(char::is_control))
            {
                return invalid(
                    &table,
                    "command entries must be non-empty text without control characters".to_owned(),
                );
            }
            let program = &probe.command[0];
            let bare = !program.contains('/') && program != "." && program != "..";
            let absolute = program.starts_with('/')
                && !Path::new(program)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir));
            if !bare && !absolute {
                return invalid(
                    &table,
                    "command[0] must be an absolute path without '..' or a bare program name resolved against the agent's PATH".to_owned(),
                );
            }
            if !(1..=MAX_DETECT_TIMEOUT_SECS).contains(&probe.timeout_secs) {
                return invalid(
                    &table,
                    format!(
                        "timeout_secs must be 1 to {MAX_DETECT_TIMEOUT_SECS} (got {})",
                        probe.timeout_secs
                    ),
                );
            }
            if probe.every_secs < MIN_DETECT_EVERY_SECS {
                return invalid(
                    &table,
                    format!(
                        "every_secs must be at least {MIN_DETECT_EVERY_SECS} (got {})",
                        probe.every_secs
                    ),
                );
            }
        }
        match (
            &self.provider_gateway_health_url,
            &self.provider_gateway_token_file,
        ) {
            (Some(url), Some(file)) => {
                validate_loopback_http_url("[agent]", "provider_gateway_health_url", url)?;
                if file.is_empty() {
                    return invalid(
                        "[agent]",
                        "provider_gateway_token_file must not be empty".to_owned(),
                    );
                }
                validate_abs_no_dotdot("[agent]", "provider_gateway_token_file", file)?;
            }
            (Some(_), None) => return invalid(
                "[agent]",
                "provider_gateway_token_file is required when provider_gateway_health_url is set"
                    .to_owned(),
            ),
            (None, Some(_)) => {
                return invalid(
                    "[agent]",
                    "provider_gateway_token_file requires provider_gateway_health_url".to_owned(),
                )
            }
            (None, None) => {}
        }
        if self.gateway_fault_markers.len() > MAX_GATEWAY_FAULT_MARKERS {
            return invalid(
                "[agent]",
                format!(
                    "gateway_fault_markers must name at most {MAX_GATEWAY_FAULT_MARKERS} markers (got {})",
                    self.gateway_fault_markers.len()
                ),
            );
        }
        for (index, marker) in self.gateway_fault_markers.iter().enumerate() {
            let table = format!("[agent] gateway_fault_markers #{index}");
            let bytes = marker.len();
            if bytes == 0 || bytes > MAX_GATEWAY_FAULT_MARKER_BYTES {
                return invalid(
                    &table,
                    format!(
                        "marker must be 1 to {MAX_GATEWAY_FAULT_MARKER_BYTES} bytes (got {bytes})"
                    ),
                );
            }
        }
        if let Some(housekeeping) = &self.housekeeping {
            housekeeping.validate()?;
        }
        Ok(())
    }
}

/// Whole-element placeholders. They replace a substring of one argv element, so no word
/// splitting is involved. Task text is deliberately not among them.
const HARNESS_WHOLE_PLACEHOLDERS: [&str; 9] = [
    "cwd",
    "task_file",
    "message",
    "job_socket",
    "result_file",
    "server",
    "provider",
    "model",
    "timeout_secs",
];
/// Fragment placeholders expand to zero or more argv elements, and only as a whole element.
const HARNESS_FRAGMENT_PLACEHOLDERS: [&str; 3] = ["model_args", "provider_args", "server_args"];
const HARNESS_MESSAGE_PLACEHOLDERS: [&str; 2] = ["task_file", "context_warning"];
const HARNESS_CWD_PLACEHOLDERS: [&str; 1] = ["cwd"];

impl AgentConfig {
    /// The built-in `opencode` profile plus operator tables, with fields merged in.
    pub fn harness_profiles(&self) -> BTreeMap<String, ResolvedHarnessProfile> {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "opencode".to_owned(),
            merge_harness_profile("opencode", &HarnessProfileConfig::default()),
        );
        for (name, table) in &self.harness_profiles {
            profiles.insert(name.clone(), merge_harness_profile(name, table));
        }
        profiles
    }

    fn validate_harness_profiles(&self) -> Result<(), ConfigError> {
        for name in self.harness_profiles.keys() {
            validate_harness_profile_name(name)?;
        }
        let merged = self.harness_profiles();
        if merged.len() > MAX_HARNESS_PROFILES {
            return invalid(
                "[agent]",
                format!(
                    "harness_profiles must name at most {MAX_HARNESS_PROFILES} profiles, including the built-in opencode profile (got {})",
                    merged.len()
                ),
            );
        }
        for (name, profile) in &merged {
            validate_resolved_harness_profile(name, profile)?;
        }
        Ok(())
    }
}

fn validate_resolved_harness_profile(
    name: &str,
    profile: &ResolvedHarnessProfile,
) -> Result<(), ConfigError> {
    let table = format!("[agent.harness_profiles.{name}]");
    if profile.argv.is_empty() || profile.argv.len() > MAX_HARNESS_ARGV {
        return invalid(
            &table,
            format!(
                "argv must hold 1 to {MAX_HARNESS_ARGV} entries (got {})",
                profile.argv.len()
            ),
        );
    }
    let argv_bytes = profile.argv.iter().map(String::len).sum::<usize>();
    if argv_bytes > MAX_HARNESS_ARGV_BYTES {
        return invalid(
            &table,
            format!(
                "argv must hold at most {MAX_HARNESS_ARGV_BYTES} bytes in total (got {argv_bytes})"
            ),
        );
    }
    for arg in &profile.argv {
        harness_text(
            &table,
            "argv",
            arg,
            &HARNESS_WHOLE_PLACEHOLDERS,
            &HARNESS_FRAGMENT_PLACEHOLDERS,
        )?;
    }
    if profile.message.is_empty()
        || profile.message.len() > MAX_HARNESS_MESSAGE_BYTES
        || profile.message.chars().any(char::is_control)
    {
        return invalid(
            &table,
            format!(
                "message must be 1 to {MAX_HARNESS_MESSAGE_BYTES} bytes without control characters (got {})",
                profile.message.len()
            ),
        );
    }
    harness_check_placeholders(
        &table,
        "message",
        &profile.message,
        &HARNESS_MESSAGE_PLACEHOLDERS,
        &[],
    )?;
    for (field, values) in [
        ("wrapper", profile.wrapper.as_slice()),
        ("model_args", profile.model_args.as_slice()),
        ("provider_args", profile.provider_args.as_slice()),
        ("server_args", profile.server_args.as_slice()),
    ] {
        if values.len() > MAX_HARNESS_FRAGMENT {
            return invalid(
                &table,
                format!(
                    "{field} must hold at most {MAX_HARNESS_FRAGMENT} entries (got {})",
                    values.len()
                ),
            );
        }
        let (allowed, fragments): (&[&str], &[&str]) = if field == "wrapper" {
            (&HARNESS_CWD_PLACEHOLDERS, &[])
        } else {
            (&HARNESS_WHOLE_PLACEHOLDERS, &HARNESS_FRAGMENT_PLACEHOLDERS)
        };
        for value in values {
            harness_text(&table, field, value, allowed, fragments)?;
        }
    }
    if profile.env.len() > MAX_HARNESS_ENV {
        return invalid(
            &table,
            format!(
                "env must name at most {MAX_HARNESS_ENV} variables (got {})",
                profile.env.len()
            ),
        );
    }
    for name in &profile.env {
        validate_harness_env_name(&table, "env", name)?;
    }
    if profile.secret_env.len() > MAX_HARNESS_SECRET_ENV {
        return invalid(
            &table,
            format!(
                "secret_env must name at most {MAX_HARNESS_SECRET_ENV} variables (got {})",
                profile.secret_env.len()
            ),
        );
    }
    for (env_name, path) in &profile.secret_env {
        validate_harness_env_name(&table, "secret_env", env_name)?;
        if path.is_empty()
            || path.len() > MAX_HARNESS_ARG_BYTES
            || path.chars().any(char::is_control)
        {
            return invalid(
                &table,
                format!(
                    "secret_env {env_name:?} must name a non-empty path without control characters"
                ),
            );
        }
    }
    if let Some(path) = &profile.result_file {
        let value = Path::new(path);
        if path.is_empty()
            || path.len() > MAX_HARNESS_RESULT_FILE_BYTES
            || path.chars().any(char::is_control)
            || value.is_absolute()
            || value.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return invalid(
                &table,
                "result_file must be a relative path without '..' or control characters".to_owned(),
            );
        }
    }
    for (field, markers) in [
        ("provider_markers", profile.provider_markers.as_slice()),
        (
            "needs_input_markers",
            profile.needs_input_markers.as_slice(),
        ),
    ] {
        if markers.len() > MAX_HARNESS_MARKERS {
            return invalid(
                &table,
                format!(
                    "{field} must hold at most {MAX_HARNESS_MARKERS} entries (got {})",
                    markers.len()
                ),
            );
        }
        for marker in markers {
            if marker.is_empty()
                || marker.len() > MAX_HARNESS_MARKER_BYTES
                || marker.chars().any(char::is_control)
            {
                return invalid(
                    &table,
                    format!("{field} entries must be 1 to {MAX_HARNESS_MARKER_BYTES} bytes without control characters"),
                );
            }
        }
    }
    if let Some(server) = &profile.server {
        if server.is_empty()
            || server.len() > MAX_HARNESS_SERVER_BYTES
            || server.starts_with('-')
            || server
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
            || !(server.starts_with("http://") || server.starts_with("https://"))
        {
            return invalid(
                &table,
                "server must be an http:// or https:// URL without whitespace or control characters, no leading '-', and at most 2048 bytes".to_owned(),
            );
        }
    }
    Ok(())
}

fn harness_text(
    table: &str,
    field: &str,
    value: &str,
    allowed: &[&str],
    fragments: &[&str],
) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > MAX_HARNESS_ARG_BYTES
        || value.chars().any(char::is_control)
    {
        return invalid(
            table,
            format!("{field} entries must be 1 to {MAX_HARNESS_ARG_BYTES} bytes without control characters"),
        );
    }
    harness_check_placeholders(table, field, value, allowed, fragments)
}

fn harness_check_placeholders(
    table: &str,
    field: &str,
    value: &str,
    whole: &[&str],
    fragments: &[&str],
) -> Result<(), ConfigError> {
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            return invalid(table, format!("{field} has an unmatched '{{'"));
        };
        let name = &after[..end];
        if name.is_empty() || name.contains('{') || name.contains('}') {
            return invalid(table, format!("{field} has a malformed placeholder"));
        }
        if whole.contains(&name) {
            rest = &after[end + 1..];
            continue;
        }
        if fragments.contains(&name) {
            if value != format!("{{{name}}}") {
                return invalid(
                    table,
                    format!("{{{name}}} must be the whole {field} element"),
                );
            }
            rest = &after[end + 1..];
            continue;
        }
        return invalid(table, format!("{field} has unknown placeholder {{{name}}}"));
    }
    if rest.contains('}') {
        return invalid(table, format!("{field} has an unmatched '}}'"));
    }
    Ok(())
}

fn validate_harness_env_name(table: &str, field: &str, name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    let first = chars.next();
    let ok = matches!(first, Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return invalid(
            table,
            format!("{field} names must match [A-Za-z_][A-Za-z0-9_]*"),
        );
    }
    if name.starts_with("LU_") || CONTROLLER_CREDENTIAL_ENV.contains(&name) {
        return invalid(
            table,
            format!("{field} may not name a controller credential or an LU_ variable"),
        );
    }
    Ok(())
}

impl HousekeepingConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.scratch_globs.len() > 8 {
            return invalid(
                "[agent.housekeeping]",
                format!(
                    "scratch_globs must name at most 8 patterns (got {})",
                    self.scratch_globs.len()
                ),
            );
        }
        for (index, pattern) in self.scratch_globs.iter().enumerate() {
            let table = format!("[agent.housekeeping] scratch_globs #{index}");
            validate_scratch_glob(&table, pattern)?;
            if let Some(parent) = Path::new(pattern).parent() {
                reject_housekeeping_root(&table, parent)?;
            }
        }
        if self.extra_roots.len() > 4 {
            return invalid(
                "[agent.housekeeping]",
                format!(
                    "extra_roots must name at most 4 roots (got {})",
                    self.extra_roots.len()
                ),
            );
        }
        for (index, root) in self.extra_roots.iter().enumerate() {
            let table = format!("[agent.housekeeping] extra_roots #{index}");
            validate_abs_no_dotdot(&table, "root", root)?;
            if root.chars().any(|ch| ch == '*' || ch == '?') {
                return invalid(
                    &table,
                    format!("root must not contain '*' or '?' (got {root:?})"),
                );
            }
            reject_housekeeping_root(&table, Path::new(root))?;
        }
        if self.min_free_percent > 100 {
            return invalid(
                "[agent.housekeeping]",
                format!(
                    "min_free_percent must be 0 to 100 (got {})",
                    self.min_free_percent
                ),
            );
        }
        if self.max_remove_per_pass == 0 {
            return invalid(
                "[agent.housekeeping]",
                "max_remove_per_pass must be at least 1".to_owned(),
            );
        }
        Ok(())
    }
}

fn validate_scratch_glob(table: &str, pattern: &str) -> Result<(), ConfigError> {
    validate_abs_no_dotdot(table, "pattern", pattern)?;
    let path = Path::new(pattern);
    let components: Vec<Component<'_>> = path.components().collect();
    let Some(Component::Normal(last)) = components.last() else {
        return invalid(
            table,
            format!("pattern must end in a file name (got {pattern:?})"),
        );
    };
    if last.to_string_lossy().contains('/') {
        return invalid(
            table,
            format!("pattern must end in a file name (got {pattern:?})"),
        );
    }
    for component in &components[..components.len() - 1] {
        if let Component::Normal(part) = component {
            if part
                .to_string_lossy()
                .chars()
                .any(|ch| ch == '*' || ch == '?')
            {
                return invalid(
                    table,
                    format!(
                        "only the final path component may contain '*' or '?' (got {pattern:?})"
                    ),
                );
            }
        }
    }
    Ok(())
}

/// Roots this broad must never be swept, even when the operator names them explicitly.
fn reject_housekeeping_root(table: &str, root: &Path) -> Result<(), ConfigError> {
    const TOO_BROAD: [&str; 5] = ["/", "/home", "/var", "/tmp", "/srv"];
    if TOO_BROAD.iter().any(|wide| root == Path::new(wide)) {
        return invalid(
            table,
            format!("housekeeping root is too broad (got {})", root.display()),
        );
    }
    Ok(())
}

fn invalid(table: &str, reason: String) -> Result<(), ConfigError> {
    Err(ConfigError::Invalid {
        table: table.to_owned(),
        reason,
    })
}

fn validate_harness_profile_name(name: &str) -> Result<(), ConfigError> {
    let table = "[agent.harness_profiles]";
    let first_ok = name
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !first_ok || !validate_name(table, name).is_ok() {
        return invalid(
            table,
            "name must be 1 to 64 characters from A-Z a-z 0-9 _ . - and start with a letter, digit or _".to_owned(),
        );
    }
    Ok(())
}

fn validate_name(table: &str, name: &str) -> Result<(), ConfigError> {
    let ok = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        invalid(
            table,
            "name must be 1 to 64 characters from A-Z a-z 0-9 . _ -".to_owned(),
        )
    }
}

/// Mirror credentials are matched against a repo's name, its full URL and its normalized id, so
/// the name may contain URL separators as well as the ordinary name alphabet.
fn validate_mirror_credential_name(table: &str, name: &str) -> Result<(), ConfigError> {
    let ok = (1..=512).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':'));
    if ok {
        Ok(())
    } else {
        invalid(
            table,
            "name must be 1 to 512 characters from A-Z a-z 0-9 . _ - / :".to_owned(),
        )
    }
}

const MAX_INTAKE_VAR_BYTES: usize = 256;

fn valid_name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn validate_pointer(table: &str, pointer: &str) -> Result<(), ConfigError> {
    let ok =
        pointer.starts_with('/') && pointer.len() <= 512 && !pointer.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        invalid(
            table,
            format!("JSON pointer {pointer:?} must start with /, be at most 512 bytes and have no control characters"),
        )
    }
}

fn validate_intake_value(table: &str, field: &str, value: &IntakeValue) -> Result<(), ConfigError> {
    match value {
        IntakeValue::Literal(text) => {
            if text.is_empty()
                || text.len() > MAX_INTAKE_VAR_BYTES
                || text.chars().any(char::is_control)
            {
                invalid(
                    table,
                    format!("{field} literal must be 1 to {MAX_INTAKE_VAR_BYTES} bytes with no control characters"),
                )
            } else {
                Ok(())
            }
        }
        IntakeValue::Pointer { pointer } => validate_pointer(table, pointer),
    }
}

/// Returns every `{{pointer}}` placeholder in an operator template.
fn template_placeholders(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        out.push(after[..end].trim().to_owned());
        rest = &after[end + 2..];
    }
    out
}

fn is_loopback_listen(listen: &str) -> bool {
    let host = listen
        .rsplit_once(':')
        .map(|(host, _)| host.trim_matches(['[', ']']))
        .unwrap_or(listen);
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'='))
        && value.bytes().filter(|b| *b == b'=').count() <= 1
}

fn valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_sha256(table: &str, value: &str) -> Result<(), ConfigError> {
    if valid_sha256_hex(value) {
        Ok(())
    } else {
        invalid(
            table,
            "token_sha256 must be 64 lowercase hex characters".to_owned(),
        )
    }
}

pub(crate) fn validate_abs_no_dotdot(
    table: &str,
    field: &str,
    value: &str,
) -> Result<(), ConfigError> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return invalid(
            table,
            format!("{field} must be an absolute path (got {value:?})"),
        );
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return invalid(
            table,
            format!("{field} must not contain '..' (got {value:?})"),
        );
    }
    Ok(())
}

fn host_is_loopback(host: &Host) -> bool {
    match host {
        Host::Name(name) => name.eq_ignore_ascii_case("localhost"),
        Host::Ip(ip) => ip.to_canonical().is_loopback(),
    }
}

fn url_err<T>(reason: String) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid {
        table: "[url]".to_owned(),
        reason,
    })
}

pub fn parse_http_url(url: &str) -> Result<ParsedUrl, ConfigError> {
    if url.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return url_err("URL must not contain control characters".to_owned());
    }
    if url.contains('@') {
        return url_err("URL must not contain credentials".to_owned());
    }
    if url.contains('?') {
        return url_err("URL must not contain a query".to_owned());
    }
    if url.contains('#') {
        return url_err("URL must not contain a fragment".to_owned());
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(|| ConfigError::Invalid {
        table: "[url]".to_owned(),
        reason: "URL must include a scheme and authority".to_owned(),
    })?;
    let https = if scheme.eq_ignore_ascii_case("https") {
        true
    } else if scheme.eq_ignore_ascii_case("http") {
        false
    } else {
        return url_err(format!("URL scheme must be http or https (got {scheme:?})"));
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return url_err("URL must include a host".to_owned());
    }
    if rest[authority_end..]
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return url_err("URL path must not contain . or .. segments".to_owned());
    }
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let close = after.find(']').ok_or_else(|| ConfigError::Invalid {
            table: "[url]".to_owned(),
            reason: "URL has an invalid IPv6 host".to_owned(),
        })?;
        let inside = &after[..close];
        let tail = &after[close + 1..];
        let port = if tail.is_empty() {
            None
        } else if let Some(port_s) = tail.strip_prefix(':') {
            if port_s.is_empty() || port_s.len() > 5 || !port_s.bytes().all(|b| b.is_ascii_digit())
            {
                return url_err("URL has an invalid port".to_owned());
            }
            let port: u16 = port_s.parse().map_err(|_| ConfigError::Invalid {
                table: "[url]".to_owned(),
                reason: "URL has an invalid port".to_owned(),
            })?;
            Some(port)
        } else {
            return url_err("URL has an invalid IPv6 host".to_owned());
        };
        let ip: Ipv6Addr = inside.parse().map_err(|_| ConfigError::Invalid {
            table: "[url]".to_owned(),
            reason: "URL has an invalid IPv6 host".to_owned(),
        })?;
        (Host::Ip(IpAddr::V6(ip)), port)
    } else {
        let (host_s, port) = if let Some((h, p)) = authority.rsplit_once(':') {
            if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
                if p.len() > 5 {
                    return url_err("URL has an invalid port".to_owned());
                }
                let port: u16 = p.parse().map_err(|_| ConfigError::Invalid {
                    table: "[url]".to_owned(),
                    reason: "URL has an invalid port".to_owned(),
                })?;
                (h, Some(port))
            } else {
                (authority, None)
            }
        } else {
            (authority, None)
        };
        if host_s.is_empty() {
            return url_err("URL must include a host".to_owned());
        }
        if host_s.len() > 253 {
            return url_err("URL host is too long".to_owned());
        }
        if host_s.starts_with('.')
            || host_s.ends_with('.')
            || host_s.starts_with('-')
            || host_s.ends_with('-')
        {
            return url_err("URL has an invalid host".to_owned());
        }
        if !host_s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return url_err("URL has an invalid host".to_owned());
        }
        let host = if let Ok(ip) = host_s.parse::<IpAddr>() {
            Host::Ip(ip)
        } else {
            Host::Name(host_s.to_owned())
        };
        (host, port)
    };
    Ok(ParsedUrl { https, host, port })
}

fn map_url_error(table: &str, field: &str, err: ConfigError, url: &str) -> ConfigError {
    match err {
        ConfigError::Invalid { reason, .. } => ConfigError::Invalid {
            table: table.to_owned(),
            reason: if let Some(stripped) = reason.strip_prefix("URL ") {
                format!("{field} {stripped}")
            } else {
                format!("{field}: {reason} (got {url:?})")
            },
        },
        other => other,
    }
}

/// The rule for a controller base URL, shared with the client: http only on loopback.
pub fn validate_controller_url(url: &str) -> Result<(), ConfigError> {
    let parsed =
        parse_http_url(url).map_err(|e| map_url_error("[agent]", "controller_url", e, url))?;
    if !parsed.https && !host_is_loopback(&parsed.host) {
        return invalid(
            "[agent]",
            "controller_url may use http only on a loopback address; use https for non-loopback hosts"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_loopback_http_url(table: &str, field: &str, url: &str) -> Result<(), ConfigError> {
    let parsed = parse_http_url(url).map_err(|e| map_url_error(table, field, e, url))?;
    if !host_is_loopback(&parsed.host) {
        return invalid(
            table,
            format!("{field} must be a loopback address (got {url:?})"),
        );
    }
    Ok(())
}

fn validate_repo_url(table: &str, url: &str, allow_file_urls: bool) -> Result<(), ConfigError> {
    if let Some(path) = url.strip_prefix("file://") {
        if !allow_file_urls {
            return invalid(
                table,
                "url must use https; file:// is allowed only with allow_file_urls = true"
                    .to_owned(),
            );
        }
        if !path.starts_with('/')
            || path.contains('@')
            || path.contains('?')
            || path.contains('#')
            || path.chars().any(char::is_control)
        {
            return invalid(table, "file url must name an absolute path".to_owned());
        }
        validate_abs_no_dotdot(table, "file url", path)?;
        return Ok(());
    }
    let parsed = parse_http_url(url).map_err(|e| map_url_error(table, "url", e, url))?;
    if !parsed.https {
        return invalid(table, "url must use https".to_owned());
    }
    Ok(())
}

/// Reads and validates a config file. Refuses files over [`MAX_CONFIG_BYTES`] and secret files
/// that group or others can read.
/// The operator's own connection settings, read from `~/.config/lu/operator.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfig {
    pub url: String,
    pub token_file: String,
    pub ca_file: Option<String>,
}

/// Where the operator file lives: `$XDG_CONFIG_HOME/lu/operator.toml`, else `$HOME/.config/...`.
pub fn operator_config_path() -> Option<std::path::PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?)
            .join(".config"),
    };
    Some(base.join("lu").join("operator.toml"))
}

/// Operator files in lookup order: the user's, then the system-wide one.
pub fn operator_config_paths() -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = operator_config_path().into_iter().collect();
    paths.push(std::path::PathBuf::from("/etc/lu/operator.toml"));
    paths
}

/// Reads an operator file. A leading `~/` in a path means the home directory.
pub fn load_operator(path: &Path) -> Result<OperatorConfig, ConfigError> {
    let text = read_config_text(path)?;
    let mut config: OperatorConfig =
        toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let home = std::env::var("HOME").unwrap_or_default();
    config.token_file = expand_home(&config.token_file, &home);
    config.ca_file = config.ca_file.map(|file| expand_home(&file, &home));
    parse_http_url(&config.url)?;
    if config.token_file.is_empty() {
        return Err(ConfigError::Parse(
            "token_file must not be empty".to_owned(),
        ));
    }
    validate_abs_no_dotdot("[operator]", "token_file", &config.token_file)?;
    if let Some(ca) = &config.ca_file {
        validate_abs_no_dotdot("[operator]", "ca_file", ca)?;
    }
    Ok(config)
}

fn expand_home(value: &str, home: &str) -> String {
    match value.strip_prefix("~/") {
        Some(rest) if !home.is_empty() => format!("{home}/{rest}"),
        _ => value.to_owned(),
    }
}

pub(crate) fn read_config_text(path: &Path) -> Result<String, ConfigError> {
    let shown = path.display().to_string();
    let file = std::fs::File::open(path).map_err(|source| ConfigError::Io {
        path: shown.clone(),
        source,
    })?;
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|source| ConfigError::Io {
            path: shown.clone(),
            source,
        })?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            path: shown,
            limit: MAX_CONFIG_BYTES,
        });
    }
    Ok(text)
}

/// Reads and validates a config file, including the secret files it names.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let config = load_values(path)?;
    check_secrets(&config)?;
    Ok(config)
}

/// Checks the secret files named by an already validated config. `lu check` deliberately does
/// not call this: it runs as a different user from the service that owns the secret files.
pub fn check_secrets(config: &Config) -> Result<(), ConfigError> {
    if let Some(tls) = &config.tls {
        secret::read_private_file("[tls] key", Path::new(&tls.key), MAX_CONFIG_BYTES)?;
    }
    if let Some(agent) = &config.agent {
        secret::read_private_file(
            "[agent] token_file",
            Path::new(&agent.token_file),
            MAX_CONFIG_BYTES,
        )?;
        if let Some(token_file) = &agent.provider_gateway_token_file {
            secret::read_private_file(
                "[agent] provider_gateway_token_file",
                Path::new(token_file),
                MAX_CONFIG_BYTES,
            )?;
        }
    }
    if let Some(publish) = &config.publish {
        for (name, remote) in &publish.remotes {
            secret::check_private_file(
                &format!("[publish.remotes.{name}] token_file"),
                Path::new(&remote.token_file),
            )?;
        }
    }
    if let Some(controller) = &config.controller {
        for repo in &controller.repos {
            if let Some(file) = &repo.credential_file {
                secret::read_private_file(
                    &format!("[[controller.repos]] ({}) credential_file", repo.name),
                    Path::new(file),
                    MAX_CONFIG_BYTES,
                )?;
            }
        }
    }
    if let Some(intake) = &config.intake {
        for source in &intake.sources {
            secret::read_private_file(
                &format!("[[intake.sources]] ({}) secret_file", source.name),
                Path::new(&source.secret_file),
                MAX_CONFIG_BYTES,
            )?;
        }
    }
    if let Some(mirror) = &config.mirror {
        if let Some(creds) = &mirror.credentials {
            for cred in &creds.repos {
                secret::check_private_file(
                    &format!("[mirror.credentials.repos] ({}) token_file", cred.name),
                    Path::new(&cred.token_file),
                )?;
            }
        }
    }
    Ok(())
}

/// Parses a config file without checking values or reading the secret files it names. For
/// commands that only print a summary and do not use the secrets.
pub fn load_structure(path: &Path) -> Result<Config, ConfigError> {
    let mut config = Config::parse_structure(&read_config_text(path)?)?;
    config.schedule_built =
        schedule::build(config.schedule.as_ref(), &config.calendar, &config.attach)?;
    Ok(config)
}

/// Reads and validates a config file without opening the secret files it names. This gives
/// `lu check` the same value validation as `lu serve`, but needs no access to TLS keys, tokens
/// or repository credentials.
pub fn load_values(path: &Path) -> Result<Config, ConfigError> {
    Config::parse(&read_config_text(path)?)
}

#[derive(Deserialize)]
struct ControllerDbConfig {
    controller: Option<ControllerDbSection>,
}

#[derive(Deserialize)]
struct ControllerDbSection {
    db: String,
}

/// Reads only `[controller] db`. No secret files are opened, so local database commands work
/// even when their user cannot read the service's TLS key or tokens.
pub fn controller_db(path: &Path) -> Result<PathBuf, ConfigError> {
    let cfg: ControllerDbConfig =
        toml::from_str(&read_config_text(path)?).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let section = cfg.controller.ok_or_else(|| ConfigError::Invalid {
        table: "[config]".to_owned(),
        reason: "the command requires a [controller] config".to_owned(),
    })?;
    validate_abs_no_dotdot("[controller]", "db", &section.db)?;
    Ok(PathBuf::from(section.db))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn scope_defaults_disable_everything_by_default() {
        let scope = ScopeConfig::default();
        assert!(!scope.enabled);
        assert_eq!(scope.max_children_per_job, 8);
        assert_eq!(scope.max_descendants_per_root, 32);
        assert_eq!(scope.max_depth, 2);
        assert_eq!(scope.max_task_bytes, 64 * 1024);
        assert_eq!(scope.max_spawn_per_minute, 6);
        assert_eq!(scope.max_artifact_read_bytes, 1024 * 1024);
        assert_eq!(scope.max_summary_bytes, 4096);
    }

    fn invalid(err: ConfigError) -> (String, String) {
        match err {
            ConfigError::Invalid { table, reason } => (table, reason),
            other => panic!("expected invalid, got {other}"),
        }
    }

    fn expect_reason(text: &str, fragment: &str) {
        let (_table, reason) = invalid(Config::parse(text).unwrap_err());
        assert!(reason.contains(fragment));
    }

    pub(super) fn controller_only() -> String {
        format!(
            r#"
[controller]
listen = "127.0.0.1:7600"
db = "/var/lib/lu/lu.db"
artifacts_dir = "/var/lib/lu/artifacts"
[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"
[[controller.operators]]
name = "ops"
token_sha256 = "{HASH_B}"
"#
        )
    }

    fn agent_only() -> String {
        r#"
[agent]
name = "peer-a"
controller_url = "https://lu.example.net:7600"
token_file = "/etc/ting/ting.token"
work_dir = "/var/lib/ting/work"
harness = "opencode"
"#
        .to_owned()
    }

    fn combined() -> String {
        format!(
            r#"
[controller]
listen = "127.0.0.1:7600"
db = "/var/lib/lu/lu.db"
artifacts_dir = "/var/lib/lu/artifacts"
lease_secs = 60
[[controller.agents]]
name = "peer-a"
token_sha256 = "{HASH_A}"
[[controller.operators]]
name = "ops"
token_sha256 = "{HASH_B}"

[agent]
name = "peer-a"
controller_url = "https://lu.example.net:7600"
token_file = "/etc/ting/ting.token"
work_dir = "/var/lib/ting/work"
harness = "opencode"
heartbeat_secs = 15
"#
        )
    }

    #[test]
    fn defaults_applied() {
        let c = Config::parse(&controller_only())
            .unwrap()
            .controller
            .unwrap();
        assert_eq!(c.lease_secs, 60);
        assert_eq!(c.max_retries, 1);
        assert_eq!(c.node_attempts, 2);
        assert_eq!(c.provider_cooldown_secs, 1800);
        assert_eq!(c.recovery_grace_secs, 30);
        assert_eq!(c.max_connections, 256);
        assert_eq!(c.max_connections_per_addr, 64);
        assert!(c.local_repo_roots.is_empty());
        let a = Config::parse(&agent_only()).unwrap().agent.unwrap();
        assert_eq!(a.heartbeat_secs, 15);
        assert_eq!(a.on_stuck, StuckPolicy::None);
        assert_eq!(a.stuck_after_secs, DEFAULT_STUCK_AFTER_SECS);
        assert_eq!(a.base_source, BaseSource::Auto);
        assert!(a.labels.is_empty());
        assert!(a.taints.is_empty());
        assert!(a.label_files.is_empty());
        let cfg = Config::parse(&combined()).unwrap();
        assert_eq!(cfg.log.level, "info");
    }

    #[test]
    fn blocked_dependent_ttl_defaults_to_a_day_and_is_validated() {
        let c = Config::parse(&controller_only())
            .unwrap()
            .controller
            .unwrap();
        assert_eq!(c.blocked_dependent_ttl, "24h");
        assert_eq!(
            c.blocked_dependent_ttl_secs(),
            DEFAULT_BLOCKED_DEPENDENT_TTL_SECS
        );
        let with_ttl = |ttl: &str| {
            controller_only().replace(
                "\n[[controller.agents]]",
                &format!("\nblocked_dependent_ttl = \"{ttl}\"\n[[controller.agents]]"),
            )
        };
        let c = Config::parse(&with_ttl("12h")).unwrap().controller.unwrap();
        assert_eq!(c.blocked_dependent_ttl_secs(), 12 * 60 * 60);
        for bad in ["0s", "nope", "999d"] {
            let text = with_ttl(bad);
            let (table, reason) = invalid(Config::parse(&text).unwrap_err());
            assert_eq!(table, "[controller]");
            assert!(reason.contains("blocked_dependent_ttl"));
        }
    }

    #[test]
    fn stack_policy_is_agent_side_fail_closed_and_strict() {
        assert!(Config::parse(&agent_only()).unwrap().stack.is_none());
        let ok = format!(
            "{}[stack]\nunion_paths = [\"CHANGELOG.md\", \"docs/**\"]\nbase_paths = [\"Cargo.lock\"]\n",
            agent_only()
        );
        let stack = Config::parse(&ok).unwrap().stack.unwrap();
        assert_eq!(stack.union_paths.len(), 2);
        assert_eq!(stack.base_paths, vec!["Cargo.lock".to_owned()]);
        assert_eq!(stack.all_paths().count(), 3);

        for bad in [
            "[stack]\nunion_paths = [\"../escape\"]\n".to_owned(),
            "[stack]\nbase_paths = [\"CHANGELOG.md\"]\nunion_paths = [\"CHANGELOG.md\"]\n"
                .to_owned(),
            "[stack]\nunknown = true\n".to_owned(),
        ] {
            let text = format!("{}{}", agent_only(), bad);
            assert!(Config::parse(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn harness_socket_owner_is_configured_by_uid_and_optional_gid() {
        let agent = Config::parse(&agent_only()).unwrap().agent.unwrap();
        assert_eq!(agent.harness_uid, None);
        assert_eq!(agent.harness_gid, None);

        let configured = Config::parse(&format!(
            "{}harness_uid = 1234\nharness_gid = 2345\n",
            agent_only()
        ))
        .unwrap()
        .agent
        .unwrap();
        assert_eq!(configured.harness_uid, Some(1234));
        assert_eq!(configured.harness_gid, Some(2345));

        let (table, reason) =
            invalid(Config::parse(&format!("{}harness_gid = 2345\n", agent_only())).unwrap_err());
        assert_eq!(table, "[agent]");
        assert_eq!(reason, "harness_gid requires harness_uid");
    }

    #[test]
    fn audit_record_is_off_by_default_and_requires_an_absolute_path() {
        assert!(Config::parse(&controller_only()).unwrap().audit.is_none());
        let base = controller_only();
        let enabled = format!("{base}\n[audit]\npath = \"/var/log/lu/audit.jsonl\"\n");
        let cfg = Config::parse(&enabled).unwrap();
        assert_eq!(cfg.audit.unwrap().path, "/var/log/lu/audit.jsonl");
        for path in ["audit.jsonl", "/var/log/lu/../lu/audit.jsonl"] {
            let text = format!("{base}\n[audit]\npath = \"{path}\"\n");
            expect_table(&text, "[audit]");
        }
        let unknown = format!("{base}\n[audit]\npath = \"/tmp/a\"\nother = 1\n");
        assert!(Config::parse(&unknown).is_err());
    }

    #[test]
    fn agent_stuck_defaults_can_be_overridden_and_are_checked() {
        let base = agent_only();
        let configured = format!("{base}on_stuck = \"cancel\"\nstuck_after_secs = 120\n");
        let agent = Config::parse(&configured).unwrap().agent.unwrap();
        assert_eq!(agent.on_stuck, StuckPolicy::Cancel);
        assert_eq!(agent.stuck_after_secs, 120);

        for value in [59, 86_401] {
            let text = format!("{base}stuck_after_secs = {value}\n");
            expect_table(&text, "[agent]");
        }
        let invalid_policy = format!("{base}on_stuck = \"later\"\n");
        assert!(Config::parse(&invalid_policy).is_err());
    }

    #[test]
    fn artifacts_scrub_defaults_to_on_with_no_extra_rules() {
        let c = Config::parse(&controller_only()).unwrap();
        assert!(c.artifacts.is_none());
        let cfg = Config::parse(&format!("{}\n[artifacts]\n", controller_only())).unwrap();
        let artifacts = cfg.artifacts.unwrap();
        assert!(artifacts.scrub);
        assert!(artifacts.extra_rules.is_empty());
        assert_eq!(artifacts, ArtifactsConfig::default());
    }

    #[test]
    fn artifacts_scrub_can_be_disabled_and_extended() {
        let text = format!(
            "{}\n[artifacts]\nscrub = false\n[[artifacts.extra_rules]]\nname = \"internal_id\"\npattern = \"ACME-[A-Z0-9]{{16}}\"\n",
            controller_only()
        );
        let artifacts = Config::parse(&text).unwrap().artifacts.unwrap();
        assert!(!artifacts.scrub);
        assert_eq!(artifacts.extra_rules.len(), 1);
        assert_eq!(artifacts.extra_rules[0].name, "internal_id");
        assert_eq!(artifacts.extra_rules[0].pattern, "ACME-[A-Z0-9]{16}");
    }

    #[test]
    fn artifact_extra_rules_are_bounded_and_named() {
        let base = format!("{}\n[artifacts]\n", controller_only());
        let rule = |name: &str, pattern: &str| {
            format!("[[artifacts.extra_rules]]\nname = \"{name}\"\npattern = \"{pattern}\"\n")
        };
        for bad in [
            "",
            "has space",
            "a/b",
            "too-long-name-aaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            expect_table(
                &format!("{base}{}", rule(bad, "x{12}")),
                &format!("[[artifacts.extra_rules]] #0 ({bad})"),
            );
        }
        expect_table(
            &format!("{}{}", base, rule("private_key", "x{12}")),
            "[[artifacts.extra_rules]] #0 (private_key)",
        );
        expect_table(
            &format!("{}{}{}", base, rule("dup", "x{12}"), rule("DUP", "y{12}")),
            "[[artifacts.extra_rules]] #1 (DUP)",
        );
        expect_table(
            &format!("{}{}", base, rule("empty", "")),
            "[[artifacts.extra_rules]] #0 (empty)",
        );
        expect_table(
            &format!("{}{}", base, rule("newline", "a\\nb")),
            "[[artifacts.extra_rules]] #0 (newline)",
        );
        expect_table(
            &format!(
                "{}{}",
                base,
                rule("huge", &"x".repeat(MAX_SCRUB_PATTERN_BYTES + 1))
            ),
            "[[artifacts.extra_rules]] #0 (huge)",
        );
        let too_many = (0..MAX_SCRUB_RULES + 1)
            .map(|i| rule(&format!("r{i}"), "x{12}"))
            .collect::<String>();
        expect_table(&format!("{base}{too_many}"), "[artifacts]");
        assert!(Config::parse(&format!("{}{}", base, rule("ok_rule", "x{12}"))).is_ok());
    }

    #[test]
    fn housekeeping_is_absent_unless_configured() {
        let a = Config::parse(&agent_only()).unwrap().agent.unwrap();
        assert!(a.housekeeping.is_none());
    }

    #[test]
    fn housekeeping_defaults_and_globs_are_checked() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n[agent.housekeeping]\n";
        let config = Config::parse(&format!("{base}scratch_globs=[\"/srv/work/target-*\"]\n"))
            .unwrap()
            .agent
            .unwrap()
            .housekeeping
            .unwrap();
        assert_eq!(config.scratch_globs, ["/srv/work/target-*"]);
        assert!(config.extra_roots.is_empty());
        assert_eq!(config.min_age_hours, 6);
        assert_eq!(config.min_free_percent, 15);
        assert_eq!(config.max_remove_per_pass, 10);

        assert!(Config::parse(&format!(
            "{base}scratch_globs=[\"/srv/work/target-?\", \"/srv/work/build-*\", \"/tmp/work/x*\"]\nextra_roots=[\"/srv/work/harness-scratch\"]\n"
        ))
        .is_ok());

        let too_many = (0..9)
            .map(|index| format!("\"/scratch-{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        expect_table(
            &format!("{base}scratch_globs=[{too_many}]\n"),
            "[agent.housekeeping]",
        );
    }

    #[test]
    fn housekeeping_globs_stay_inside_their_parent() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n[agent.housekeeping]\n";
        for pattern in ["relative/*", "/srv/../work/*", "/srv/*/target"] {
            expect_table(
                &format!("{base}scratch_globs=[\"{pattern}\"]\n"),
                "[agent.housekeeping] scratch_globs #0",
            );
        }
        expect_table(
            &format!("{base}scratch_globs=[\"/srv/work/target-*\"]\nmin_free_percent=101\n"),
            "[agent.housekeeping]",
        );
        expect_table(
            &format!("{base}scratch_globs=[\"/srv/work/target-*\"]\nmax_remove_per_pass=0\n"),
            "[agent.housekeeping]",
        );
    }

    #[test]
    fn housekeeping_refuses_roots_that_are_too_broad() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n[agent.housekeeping]\n";
        for wide in ["/", "/home", "/var", "/tmp", "/srv"] {
            expect_table(
                &format!("{base}scratch_globs=[\"{wide}/target-*\"]\n"),
                "[agent.housekeeping] scratch_globs #0",
            );
            expect_table(
                &format!("{base}extra_roots=[\"{wide}\"]\n"),
                "[agent.housekeeping] extra_roots #0",
            );
        }
    }

    #[test]
    fn housekeeping_extra_roots_stay_absolute_and_literal() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n[agent.housekeeping]\n";
        for root in ["work/scratch", "/srv/../work/scratch", "/srv/work/*"] {
            expect_table(
                &format!("{base}extra_roots=[\"{root}\"]\n"),
                "[agent.housekeeping] extra_roots #0",
            );
        }
        let too_many = (0..5)
            .map(|index| format!("\"/scratch-{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        expect_table(
            &format!("{base}extra_roots=[{too_many}]\n"),
            "[agent.housekeeping]",
        );
    }

    #[test]
    fn at_least_one_table_required() {
        let (t, _) = invalid(Config::parse("").unwrap_err());
        assert_eq!(t, "[config]");
    }

    #[test]
    fn unknown_keys_refused() {
        assert!(Config::parse(&format!("{}\n[nope]\n", controller_only())).is_err());
        let e = Config::parse(
            "[controller]\nlisten = \"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\nextra=1\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("extra"), "{e}");
    }

    fn publish_block() -> String {
        format!(
            "{}\n[publish.remotes.forge]\nurl = \"https://forge.invalid/lu.git\"\ntoken_file = \"/tmp/lu-token\"\nmax_pushes_per_hour = 5\n",
            controller_only()
        )
    }

    #[test]
    fn publish_requires_https_and_no_embedded_credentials() {
        assert!(Config::parse(&publish_block()).is_ok());
        let bad = publish_block().replace("https://forge.invalid", "http://forge.invalid");
        expect_table(&bad, "[publish.remotes]");
        let bad = publish_block().replace("forge.invalid", "user:pw@forge.invalid");
        expect_table(&bad, "[publish.remotes]");
        let bad = publish_block() + "branch_prefixes = [\"refs/heads/\"]\n";
        expect_table(&bad, "[publish.remotes]");
    }

    #[test]
    fn publish_gate_values_are_checked() {
        let base = publish_block() + "[publish.gates.secrets]\ncommand = \"rg secret\"\n";
        assert!(Config::parse(&format!(
            "{base}timeout_secs = 60\nexpected_exit_code = 1\n"
        ))
        .is_ok());
        expect_table(
            &format!("{base}timeout_secs = 0\nexpected_exit_code = 0\n"),
            "[publish.gates]",
        );
        expect_table(
            &format!("{base}timeout_secs = 60\nexpected_exit_code = 256\n"),
            "[publish.gates]",
        );
        expect_table(
            &format!("{base}timeout_secs = 60\nexpected_exit_code = 0\nextra = 1\n"),
            "[publish.gates]",
        );
    }

    #[test]
    fn publish_test_transport_requires_shim_url() {
        let base = publish_block() + "test_file_url = \"file:///tmp/remote.git\"\n";
        expect_table(&base, "[publish.remotes]");
        let ok = publish_block().replace("forge.invalid", "shim.invalid")
            + "test_file_url = \"file:///tmp/remote.git\"\n";
        assert!(Config::parse(&ok).is_ok());
    }

    fn expect_table(text: &str, table: &str) {
        match Config::parse(text).unwrap_err() {
            ConfigError::Invalid { table: t, .. } => assert_eq!(t, table),
            ConfigError::Parse(message) => {
                assert!(
                    message.contains("unknown field") || message.contains("extra"),
                    "unexpected parse error: {message}"
                );
            }
            other => panic!("expected invalid, got {other}"),
        }
    }

    #[test]
    fn controller_range_checks_name_table() {
        let base = |field: &str, val: &str| {
            format!(
            "[controller]\nlisten = \"127.0.0.1:7600\"\ndb = \"/db\"\nartifacts_dir = \"/a\"\n{field} = {val}\n"
        )
        };
        for v in ["4", "3601"] {
            expect_table(&base("lease_secs", v), "[controller]");
        }
        expect_table(&base("max_retries", "4"), "[controller]");
        for v in ["0", "5"] {
            expect_table(&base("node_attempts", v), "[controller]");
        }
        for v in ["59", "86401"] {
            expect_table(&base("provider_cooldown_secs", v), "[controller]");
        }
        expect_table(&base("recovery_grace_secs", "601"), "[controller]");
        expect_table(&base("max_connections", "0"), "[controller]");
        expect_table(&base("max_connections_per_addr", "0"), "[controller]");
        expect_table(
            "[controller]\nlisten = \"127.0.0.1:7600\"\ndb = \"/db\"\nartifacts_dir = \"/a\"\nmax_connections = 4\nmax_connections_per_addr = 5\n",
            "[controller]",
        );
        // bounds ok
        assert!(Config::parse(&base("lease_secs", "5")).is_ok());
        assert!(Config::parse(&base("lease_secs", "3600")).is_ok());
        assert!(Config::parse(&base("max_retries", "0")).is_ok());
        assert!(Config::parse(&base("max_retries", "3")).is_ok());
        assert!(Config::parse(&base("node_attempts", "1")).is_ok());
        assert!(Config::parse(&base("node_attempts", "4")).is_ok());
        assert!(Config::parse(&base("provider_cooldown_secs", "60")).is_ok());
        assert!(Config::parse(&base("provider_cooldown_secs", "86400")).is_ok());
        assert!(Config::parse(&base("recovery_grace_secs", "0")).is_ok());
        assert!(Config::parse(&base("recovery_grace_secs", "600")).is_ok());
    }

    #[test]
    fn agent_range_checks_name_table() {
        let base = |field: &str, val: &str| {
            format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n{field}={val}\n"
        )
        };
        for v in ["1", "301"] {
            expect_table(&base("heartbeat_secs", v), "[agent]");
        }
        assert!(Config::parse(&base("heartbeat_secs", "2")).is_ok());
        assert!(Config::parse(&base("heartbeat_secs", "300")).is_ok());
    }

    fn agent_with_provider_models(pairs: &[(&str, &str)]) -> String {
        let mut text = String::from(
            "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n",
        );
        for (provider, model) in pairs {
            text.push_str(&format!(
                "[[agent.provider_models]]\nprovider=\"{provider}\"\nmodel=\"{model}\"\n"
            ));
        }
        text
    }

    #[test]
    fn provider_models_keep_configuration_order() {
        let cfg = Config::parse(&agent_with_provider_models(&[
            ("anthropic", "claude-sonnet-4-5"),
            ("openai", "gpt-5"),
        ]))
        .unwrap();
        let agent = cfg.agent.unwrap();
        assert_eq!(agent.provider_models.len(), 2);
        assert_eq!(agent.provider_models[0].provider, "anthropic");
        assert_eq!(agent.provider_models[0].model, "claude-sonnet-4-5");
        assert_eq!(agent.provider_models[1].provider, "openai");
        assert_eq!(agent.provider_models[1].model, "gpt-5");
    }

    #[test]
    fn provider_models_validate_names_models_and_duplicates() {
        expect_table(
            &agent_with_provider_models(&[("bad name", "m")]),
            "[agent.provider_models] #1",
        );
        expect_table(
            &agent_with_provider_models(&[("good", "")]),
            "[agent.provider_models] #1",
        );
        expect_table(
            &agent_with_provider_models(&[("good", &"m".repeat(MAX_MODEL_BYTES + 1))]),
            "[agent.provider_models] #1",
        );
        expect_table(
            &agent_with_provider_models(&[("dup", "a"), ("dup", "b")]),
            "[agent.provider_models] #2",
        );
    }

    #[test]
    fn provider_models_have_a_count_limit() {
        let pairs: Vec<(String, String)> = (0..MAX_PROVIDER_MODELS + 1)
            .map(|i| (format!("p{i}"), "m".to_owned()))
            .collect();
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(p, m)| (p.as_str(), m.as_str()))
            .collect();
        expect_table(&agent_with_provider_models(&refs), "[agent]");
    }

    #[test]
    fn the_builtin_profile_is_always_available() {
        let agent = Config::parse(&agent_only()).unwrap().agent.unwrap();
        let profiles = agent.harness_profiles();
        let builtin = profiles.get("opencode").expect("built-in profile");
        assert_eq!(builtin.argv[0], "opencode");
        assert!(builtin.secret_env.is_empty());
        assert!(builtin.server.is_none());
        assert_eq!(builtin.stdin, HarnessStdin::Null);
        assert_eq!(builtin.provider_markers.len(), 5);
    }

    #[test]
    fn a_profile_override_merges_individual_fields() {
        let text = format!(
            "{}\n[agent.harness_profiles.opencode]\nserver=\"https://example.net\"\n",
            agent_only()
        );
        let agent = Config::parse(&text).unwrap().agent.unwrap();
        let builtin = agent.harness_profiles();
        let profile = &builtin["opencode"];
        assert_eq!(profile.server.as_deref(), Some("https://example.net"));
        assert_eq!(profile.argv[0], "opencode");
        assert_eq!(
            profile.message,
            "Follow the task in the provided file.{context_warning}"
        );
        assert_eq!(profile.provider_markers.len(), 5);
    }

    #[test]
    fn a_workspace_group_is_a_plain_group_name_or_number() {
        let base = |val: &str| {
            format!(
                "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\nworkspace_group={val}\n"
            )
        };
        for ok in ["\"opencode\"", "\"1001\"", "\"dev.group-1_a\""] {
            assert!(Config::parse(&base(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "\"\"",
            "\"has space\"",
            "\"a/b\"",
            "\"semi;colon\"",
            "\"colon:x\"",
            "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
        ] {
            expect_table(&base(bad), "[agent]");
        }
    }

    #[test]
    fn labels_and_label_files_are_checked() {
        let base = |field: &str, val: &str| {
            format!(
                "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n{field}={val}\n"
            )
        };
        assert!(Config::parse(&base("labels", r#"["php8.3", "region=eu"]"#)).is_ok());
        assert!(Config::parse(&base("label_files", r#"["/etc/lu/labels"]"#)).is_ok());
        for bad in [r#"["bad label"]"#, r#"["a=b=c"]"#, r#"[""]"#] {
            expect_table(&base("labels", bad), "[agent]");
        }
        let too_many = format!(
            "[{}]",
            (0..=32)
                .map(|i| format!("\"label-{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        expect_table(&base("labels", &too_many), "[agent]");
        let too_many_files = format!(
            "[{}]",
            (0..=8)
                .map(|i| format!("\"/l/{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        expect_table(&base("label_files", &too_many_files), "[agent]");
        for bad in [r#"["bad label"]"#, r#"["a=b=c"]"#, r#"[""]"#] {
            expect_table(&base("taints", bad), "[agent]");
        }
        let too_many_taints = format!(
            "[{}]",
            (0..=8)
                .map(|i| format!("\"taint-{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        expect_table(&base("taints", &too_many_taints), "[agent]");
        assert!(Config::parse(&base("taints", r#"["lab"]"#)).is_ok());
        for bad in [r#"["relative/labels"]"#, r#"["/srv/../labels"]"#] {
            expect_table(&base("label_files", bad), "[agent] label_files #0");
        }
    }

    #[test]
    fn detect_entries_are_validated() {
        let base = |table: &str| {
            format!(
                "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n{table}"
            )
        };
        let entry = |label: &str, command: &str| {
            format!("[[agent.detect]]\nlabel={label}\ncommand={command}\n")
        };
        assert!(Config::parse(&base(&entry(
            "\"rust\"",
            "[\"/usr/bin/cargo\", \"--version\"]"
        )))
        .is_ok());
        assert!(Config::parse(&base(&entry("\"rust\"", "[\"cargo\", \"--version\"]"))).is_ok());

        for bad in ["\"bad label\"", "\"a=b=c\"", "\"\""] {
            expect_table(&base(&entry(bad, "[\"/bin/true\"]")), "[agent.detect] #1");
        }
        for bad in [
            "[]",
            "[\"\"]",
            "[\"/bin/true\", \"\"]",
            "[\"/bin/true\", \"bad\\narg\"]",
            "[\"../bin/true\"]",
            "[\"/srv/../bin/true\"]",
        ] {
            expect_table(&base(&entry("\"rust\"", bad)), "[agent.detect] #1");
        }
        let too_many_args = format!(
            "[{}]",
            (0..=8).map(|_| "\"arg\"").collect::<Vec<_>>().join(",")
        );
        expect_table(
            &base(&entry("\"rust\"", &too_many_args)),
            "[agent.detect] #1",
        );
        for bad in ["0", "31"] {
            expect_table(
                &base(&format!(
                    "{}timeout_secs={bad}\n",
                    entry("\"rust\"", "[\"/bin/true\"]")
                )),
                "[agent.detect] #1",
            );
        }
        expect_table(
            &base(&format!(
                "{}every_secs=29\n",
                entry("\"rust\"", "[\"/bin/true\"]")
            )),
            "[agent.detect] #1",
        );
        assert!(Config::parse(&base(&format!(
            "{}timeout_secs=30\nevery_secs=30\n",
            entry("\"rust\"", "[\"/bin/true\"]")
        )))
        .is_ok());

        let duplicate = format!(
            "{}{}",
            entry("\"rust\"", "[\"/bin/true\"]"),
            entry("\"rust\"", "[\"/bin/false\"]")
        );
        expect_table(&base(&duplicate), "[agent.detect] #2");
        let too_many = (0..=16)
            .map(|i| entry(&format!("\"probe-{i}\""), "[\"/bin/true\"]"))
            .collect::<String>();
        expect_table(&base(&too_many), "[agent]");
        let unknown = format!("{}surprise=true\n", entry("\"rust\"", "[\"/bin/true\"]"));
        assert!(Config::parse(&base(&unknown)).is_err());
    }

    #[test]
    fn detect_defaults_are_applied() {
        let text = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n[[agent.detect]]\nlabel=\"rust\"\ncommand=[\"/bin/true\"]\n";
        let agent = Config::parse(text).unwrap().agent.unwrap();
        assert!(!agent.detect.is_empty());
        assert_eq!(agent.detect[0].timeout_secs, 5);
        assert_eq!(agent.detect[0].every_secs, 300);
        assert!(agent_only_defaults().detect.is_empty());
    }

    fn agent_only_defaults() -> AgentConfig {
        Config::parse(&agent_only()).unwrap().agent.unwrap()
    }

    #[test]
    fn name_validation_and_uniqueness() {
        let bad_name = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.agents]]\nname=\"bad name\"\ntoken_sha256=\"{HASH_A}\"\n"
        );
        expect_table(&bad_name, "[[controller.agents]] #1 (bad name)");
        let dup = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.agents]]\nname=\"a\"\ntoken_sha256=\"{HASH_A}\"\n[[controller.agents]]\nname=\"a\"\ntoken_sha256=\"{HASH_B}\"\n"
        );
        let (t, r) = invalid(Config::parse(&dup).unwrap_err());
        assert_eq!(t, "[[controller.agents]] #2 (a)");
        assert!(r.contains("earlier agent"));
        let dup_op = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.operators]]\nname=\"o\"\ntoken_sha256=\"{HASH_A}\"\n[[controller.operators]]\nname=\"o\"\ntoken_sha256=\"{HASH_B}\"\n"
        );
        let (t, r) = invalid(Config::parse(&dup_op).unwrap_err());
        assert_eq!(t, "[[controller.operators]] #2 (o)");
        assert!(r.contains("earlier operator"));
    }

    #[test]
    fn token_hash_shape_and_reuse() {
        let bad = "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.agents]]\nname=\"a\"\ntoken_sha256=\"ABC\"\n";
        expect_table(bad, "[[controller.agents]] #1 (a)");
        // reuse within agents
        let reuse_a = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.agents]]\nname=\"a\"\ntoken_sha256=\"{HASH_A}\"\n[[controller.agents]]\nname=\"b\"\ntoken_sha256=\"{HASH_A}\"\n"
        );
        expect_table(&reuse_a, "[[controller.agents]] #2 (b)");
        // reuse within operators
        let reuse_o = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.operators]]\nname=\"a\"\ntoken_sha256=\"{HASH_A}\"\n[[controller.operators]]\nname=\"b\"\ntoken_sha256=\"{HASH_A}\"\n"
        );
        expect_table(&reuse_o, "[[controller.operators]] #2 (b)");
    }

    #[test]
    fn cross_scope_hash_reuse_refused() {
        let text = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.agents]]\nname=\"a\"\ntoken_sha256=\"{HASH_A}\"\n[[controller.operators]]\nname=\"o\"\ntoken_sha256=\"{HASH_A}\"\n"
        );
        let (t, r) = invalid(Config::parse(&text).unwrap_err());
        assert!(t.starts_with("[[controller.operators]]"));
        assert!(r.contains("both scopes") || r.contains("same as one used"));
    }

    #[test]
    fn non_loopback_listen_without_tls_refused() {
        let text = "[controller]\nlisten=\"0.0.0.0:7600\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n";
        expect_table(text, "[controller]");
        let ok = "[controller]\nlisten=\"0.0.0.0:7600\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[tls]\ncert=\"/c\"\nkey=\"/k\"\n";
        assert!(Config::parse(ok).is_ok());
        let loopback = "[controller]\nlisten=\"[::1]:7600\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n";
        assert!(Config::parse(loopback).is_ok());
    }

    #[test]
    fn controller_url_rules() {
        let base = |url: &str| {
            format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"{url}\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n"
        )
        };
        expect_table(&base("http://example.net:7600"), "[agent]");
        assert!(Config::parse(&base("http://127.0.0.1:7600")).is_ok());
        assert!(Config::parse(&base("http://localhost:7600")).is_ok());
        assert!(Config::parse(&base("https://example.net:7600")).is_ok());
        expect_table(&base("https://user:pw@example.net"), "[agent]");
        expect_table(&base("https://example.net?q=1"), "[agent]");
        expect_table(&base("https://example.net#f"), "[agent]");
        expect_reason(
            &base("https://example.net/a/../b"),
            "controller_url path must not contain . or .. segments",
        );
        assert!(Config::parse(&base("https://example.net/a/b")).is_ok());
    }

    #[test]
    fn url_host_parsing_strict_cases() {
        let agent = |url: &str| {
            format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"{url}\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n"
        )
        };
        let gateway = |url: &str| {
            format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\nprovider_gateway_health_url=\"{url}\"\nprovider_gateway_token_file=\"/rt\"\n"
        )
        };
        // refused
        for url in [
            "http://[::1].attacker.example:80",
            "http://[::1]x",
            "http://[::1]:80x",
            "http://[::1]:99999",
            "http://127.0.0.1.evil.example",
            "http://localhost.evil.example",
            "http://user@127.0.0.1",
            "http://2130706433",
        ] {
            expect_table(&agent(url), "[agent]");
            expect_table(&gateway(url), "[agent]");
        }
        // accepted
        for url in [
            "http://[::ffff:127.0.0.1]:8080",
            "http://localhost:4100",
            "http://127.0.0.1:4100",
            "http://[::1]:4100",
        ] {
            assert!(Config::parse(&agent(url)).is_ok(), "agent accepted: {url}");
            assert!(
                Config::parse(&gateway(url)).is_ok(),
                "gateway accepted: {url}"
            );
        }
        assert!(
            Config::parse(&agent("https://lu.example.net:7600")).is_ok(),
            "https accepted"
        );
        assert!(
            Config::parse(&agent("HTTPS://lu.example.net:7600")).is_ok(),
            "uppercase scheme"
        );
        assert!(
            Config::parse(&agent("HTTP://127.0.0.1:7600")).is_ok(),
            "uppercase http loopback"
        );
        // empty host
        expect_table(&agent("http:///path"), "[agent]");
        expect_table(&agent("https://"), "[agent]");
        // 300 byte host
        let long_host = format!("https://{}.example.net", "a".repeat(300));
        expect_table(&agent(&long_host), "[agent]");
        // control character in URL
        expect_table(&agent("http://127.0.0.1/\\u007f"), "[agent]");
        expect_table(&agent("http://exa\\u0001mple.net"), "[agent]");
    }

    #[test]
    fn paths_must_be_absolute_and_no_dotdot() {
        let c = |db: &str| {
            format!("[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"{db}\"\nartifacts_dir=\"/a\"\n")
        };
        expect_table(&c("relative/db"), "[controller]");
        expect_table(&c("/var/lib/../db"), "[controller]");
        let a = |w: &str| {
            format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"{w}\"\nharness=\"opencode\"\n"
        )
        };
        expect_table(&a("rel"), "[agent]");
        expect_table(&a("/srv/../w"), "[agent]");
        expect_table(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"rel\"\n",
            "[controller]",
        );
    }

    #[test]
    fn local_repo_roots_must_be_absolute() {
        let base = "[controller]\nlisten=\"127.0.0.1:7600\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n";
        let ok = format!("{base}local_repo_roots=[\"/srv/git\", \"/srv/mirrors\"]\n");
        let cfg = Config::parse(&ok).unwrap().controller.unwrap();
        assert_eq!(cfg.local_repo_roots, ["/srv/git", "/srv/mirrors"]);
        expect_table(
            &format!("{base}local_repo_roots=[\"relative\"]\n"),
            "[controller] local_repo_roots #0",
        );
        expect_table(
            &format!("{base}local_repo_roots=[\"/srv/../git\"]\n"),
            "[controller] local_repo_roots #0",
        );
    }

    #[test]
    fn base_source_accepts_the_three_modes() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n";
        for (text, want) in [
            ("clone", BaseSource::Clone),
            ("controller", BaseSource::Controller),
            ("auto", BaseSource::Auto),
        ] {
            let cfg = Config::parse(&format!("{base}base_source=\"{text}\"\n")).unwrap();
            assert_eq!(cfg.agent.unwrap().base_source, want, "{text}");
        }
        let err = Config::parse(&format!("{base}base_source=\"mirror\"\n")).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "{err}");
        assert!(err.to_string().contains("mirror"), "{err}");
    }

    #[test]
    fn heartbeat_vs_lease_when_combined() {
        let bad = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\nlease_secs=30\n[[controller.agents]]\nname=\"a\"\ntoken_sha256=\"{HASH_A}\"\n[[controller.operators]]\nname=\"o\"\ntoken_sha256=\"{HASH_B}\"\n[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\nheartbeat_secs=15\n"
        );
        expect_table(&bad, "[agent]");
        // equal is not allowed (< required)
        let eq = bad.to_string();
        assert!(Config::parse(&eq).is_err());
        let ok = bad.replace("lease_secs=30", "lease_secs=31");
        assert!(Config::parse(&ok).is_ok());
    }

    #[test]
    fn provider_gateway_rules() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n";
        expect_table(
            &format!("{base}provider_gateway_health_url=\"http://127.0.0.1:4100/v1/health\"\n"),
            "[agent]",
        );
        assert!(Config::parse(&format!("{base}provider_gateway_health_url=\"http://127.0.0.1:4100/v1/health\"\nprovider_gateway_token_file=\"/rt\"\n")).is_ok());
        expect_table(&format!("{base}provider_gateway_health_url=\"http://example.net:4100/v1/health\"\nprovider_gateway_token_file=\"/rt\"\n"), "[agent]");
        expect_table(
            &format!("{base}provider_gateway_token_file=\"/rt\"\n"),
            "[agent]",
        );
        for file in ["relative/token", "/srv/../token"] {
            expect_reason(
                &format!("{base}provider_gateway_health_url=\"http://127.0.0.1:4100/v1/health\"\nprovider_gateway_token_file=\"{file}\"\n"),
                "provider_gateway_token_file must",
            );
        }
    }

    #[test]
    fn intake_rule_bounds_match_the_job_spec() {
        let rule = |field: &str, value: u64| {
            format!(
                "{}\n[intake]\n[[intake.sources]]\nname=\"events\"\nsecret_file=\"/etc/lu/events.key\"\n[[intake.sources.rules]]\nrepo=\"https://example.net/a.git\"\nref=\"refs/heads/main\"\n{field}={value}\n",
                controller_only()
            )
        };
        expect_reason(&rule("priority", 10), "priority must be 0 to 9");
        expect_reason(&rule("timeout", 0), "timeout must be 1 to 86400 seconds");
        expect_reason(
            &rule("timeout", 86_401),
            "timeout must be 1 to 86400 seconds",
        );
        assert!(Config::parse(&rule("priority", 0)).is_ok());
        assert!(Config::parse(&rule("priority", 9)).is_ok());
        assert!(Config::parse(&rule("timeout", 1)).is_ok());
        assert!(Config::parse(&rule("timeout", 86_400)).is_ok());
    }

    #[test]
    fn every_configured_path_is_validated() {
        for file in ["relative/file", "/srv/../file"] {
            expect_reason(
                &format!("[log]\nfile=\"{file}\"\n\n{}", controller_only()),
                "file must",
            );
            expect_reason(
                &format!(
                    "{}\n[tls]\ncert=\"{file}\"\nkey=\"/key\"\n",
                    controller_only()
                ),
                "cert must",
            );
            expect_reason(
                &format!(
                    "{}\n[[controller.repos]]\nname=\"mirror\"\nurl=\"https://example.net/a.git\"\ncredential_file=\"{file}\"\n",
                    controller_only()
                ),
                "credential_file must",
            );
            expect_reason(
                &format!(
                    "{}\n[mirror.credentials]\n[[mirror.credentials.repos]]\nname=\"mirror\"\ntoken_file=\"{file}\"\n",
                    controller_only()
                ),
                "token_file must",
            );
            expect_reason(
                &format!(
                    "{}\n[intake]\n[[intake.sources]]\nname=\"events\"\nsecret_file=\"{file}\"\n[[intake.sources.rules]]\nrepo=\"https://example.net/a.git\"\nref=\"refs/heads/main\"\n",
                    controller_only()
                ),
                "secret_file must",
            );
            let agent = |field: &str| {
                format!(
                    "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n{field}=\"{file}\"\n"
                )
            };
            expect_reason(
                &format!(
                    "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"{file}\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n"
                ),
                "token_file must",
            );
            expect_reason(&agent("ca_file"), "ca_file must");
            for removed in [
                "opencode_server",
                "opencode_password_file",
                "harness_wrapper",
            ] {
                assert!(
                    matches!(Config::parse(&agent(removed)), Err(ConfigError::Parse(_))),
                    "{removed} should be an unknown key"
                );
            }
        }
        expect_reason(
            &format!(
                "{}\n[[controller.repos]]\nname=\"mirror\"\nurl=\"file:///srv/../repo.git\"\nallow_file_urls=true\n",
                controller_only()
            ),
            "file url must not contain '..'",
        );
    }

    #[test]
    fn operator_config_paths_and_url_are_validated() {
        let path = std::env::temp_dir().join(format!("lu-operator-{}", std::process::id()));
        let cases = [
            (
                "url = \"https://example.net\"\ntoken_file = \"relative\"\n",
                "token_file must be an absolute path",
            ),
            (
                "url = \"https://example.net\"\ntoken_file = \"/t\"\nca_file = \"/srv/../ca.pem\"\n",
                "ca_file must not contain '..'",
            ),
            (
                "url = \"https://example.net/a/../b\"\ntoken_file = \"/t\"\n",
                "path must not contain . or .. segments",
            ),
        ];
        for (text, expected) in cases {
            std::fs::write(&path, text).unwrap();
            let err = load_operator(&path).unwrap_err();
            match err {
                ConfigError::Invalid { reason, .. } => assert!(
                    reason.contains(expected),
                    "expected {expected:?}, got {reason:?}"
                ),
                ConfigError::Parse(message) => assert!(
                    message.contains(expected),
                    "expected {expected:?}, got {message:?}"
                ),
                other => panic!("expected config error, got {other}"),
            }
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn former_provider_gateway_keys_are_refused() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n";
        for key in [
            "reiki_health_url=\"http://127.0.0.1:4100/v1/health\"",
            "reiki_token_file=\"/rt\"",
        ] {
            let text = format!("{base}{key}\n");
            match Config::parse(&text) {
                Err(ConfigError::Parse(message)) => assert!(
                    message.contains("unknown field")
                        && message.contains(key.split_once('=').unwrap().0),
                    "expected an unknown-field error, got {message}"
                ),
                other => panic!("expected a parse error, got {other:?}"),
            }
        }
    }

    #[test]
    fn gateway_fault_marker_rules() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n";
        let default = Config::parse(base).unwrap().agent.unwrap();
        assert!(default.gateway_fault_markers.is_empty());

        let ok = Config::parse(&format!(
            "{base}gateway_fault_markers=[\"Store Offline\", \"capacity exhausted\"]\n"
        ))
        .unwrap()
        .agent
        .unwrap();
        assert_eq!(
            ok.gateway_fault_markers,
            ["Store Offline", "capacity exhausted"]
        );

        let at_limit = format!(
            "{base}gateway_fault_markers=[{}]\n",
            (0..MAX_GATEWAY_FAULT_MARKERS)
                .map(|i| format!("\"m{i}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(Config::parse(&at_limit).is_ok());
        let too_many = format!(
            "{base}gateway_fault_markers=[{}]\n",
            (0..=MAX_GATEWAY_FAULT_MARKERS)
                .map(|i| format!("\"m{i}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        expect_table(&too_many, "[agent]");
        assert!(Config::parse(&too_many)
            .unwrap_err()
            .to_string()
            .contains("at most 16"));

        let max_bytes = "x".repeat(MAX_GATEWAY_FAULT_MARKER_BYTES);
        assert!(Config::parse(&format!("{base}gateway_fault_markers=[\"{max_bytes}\"]\n")).is_ok());
        let too_long = "x".repeat(MAX_GATEWAY_FAULT_MARKER_BYTES + 1);
        expect_table(
            &format!("{base}gateway_fault_markers=[\"{too_long}\"]\n"),
            "[agent] gateway_fault_markers #0",
        );
        expect_table(
            &format!("{base}gateway_fault_markers=[\"\"]\n"),
            "[agent] gateway_fault_markers #0",
        );
    }

    #[test]
    fn repo_urls_names_and_refs_are_validated() {
        let base = |body: &str| {
            format!(
                "[controller]\nlisten=\"127.0.0.1:7600\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.repos]]\n{body}"
            )
        };
        assert!(Config::parse(&base(
            "name=\"mirror\"\nurl=\"https://example.net/team/repo.git\"\n"
        ))
        .is_ok());
        expect_table(
            &base("name=\"mirror\"\nurl=\"http://example.net/team/repo.git\"\n"),
            "[[controller.repos]] #1 (mirror)",
        );
        expect_table(
            &base("name=\"mirror\"\nurl=\"https://user:pw@example.net/team/repo.git\"\n"),
            "[[controller.repos]] #1 (mirror)",
        );
        expect_table(
            &base("name=\"mirror\"\nurl=\"file:///srv/repo\"\n"),
            "[[controller.repos]] #1 (mirror)",
        );
        assert!(Config::parse(&base(
            "name=\"mirror\"\nurl=\"file:///srv/repo\"\nallow_file_urls=true\n"
        ))
        .is_ok());
        let dup = format!(
            "{}{}",
            base("name=\"mirror\"\nurl=\"https://example.net/a.git\"\n"),
            "[[controller.repos]]\nname=\"mirror\"\nurl=\"https://example.net/b.git\"\n"
        );
        let (table, reason) = invalid(Config::parse(&dup).unwrap_err());
        assert_eq!(table, "[[controller.repos]] #2 (mirror)");
        assert!(reason.contains("earlier repository"));
        expect_table(
            &base("name=\"mirror\"\nurl=\"https://example.net/a.git\"\nrefs=[]\n"),
            "[[controller.repos]] #1 (mirror)",
        );
    }

    #[test]
    fn group_names_and_limits_are_validated() {
        let base = |body: &str| {
            format!(
                "[controller]\nlisten=\"127.0.0.1:7600\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.groups]]\n{body}"
            )
        };
        assert!(Config::parse(&base("name=\"nightly\"\nmax_running=2\n")).is_ok());
        expect_table(
            &base("name=\"bad name\"\nmax_running=2\n"),
            "[[controller.groups]] #1 (bad name)",
        );
        expect_table(
            &base("name=\"nightly\"\nmax_running=0\n"),
            "[[controller.groups]] #1 (nightly)",
        );
        expect_table(
            &base("name=\"nightly\"\nmax_running=65\n"),
            "[[controller.groups]] #1 (nightly)",
        );
        assert!(Config::parse(&base("name=\"nightly\"\n")).is_err());
        let dup = format!(
            "{}{}",
            base("name=\"nightly\"\nmax_running=1\n"),
            "[[controller.groups]]\nname=\"nightly\"\nmax_running=2\n"
        );
        let (table, reason) = invalid(Config::parse(&dup).unwrap_err());
        assert_eq!(table, "[[controller.groups]] #2 (nightly)");
        assert!(reason.contains("earlier group"));
    }

    #[test]
    fn a_repo_credential_file_must_be_private_on_load() {
        let dir = std::env::temp_dir().join(format!("lu-cfg-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cred = dir.join("credential");
        std::fs::write(&cred, "user:password").unwrap();
        std::fs::set_permissions(&cred, std::fs::Permissions::from_mode(0o644)).unwrap();
        let cfg = dir.join("c.toml");
        std::fs::write(
            &cfg,
            format!(
                "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.repos]]\nname=\"mirror\"\nurl=\"https://example.net/a.git\"\ncredential_file=\"{}\"\n",
                cred.display()
            ),
        )
        .unwrap();
        assert!(matches!(load(&cfg), Err(ConfigError::InsecureFile { .. })));
        std::fs::set_permissions(&cred, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&cfg).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn example_file_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lu.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        Config::parse(&text).unwrap();
    }

    #[test]
    fn token_file_mode_0644_refused_on_load() {
        let dir = std::env::temp_dir().join(format!("lu-cfg-tok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tok = dir.join("agent.token");
        std::fs::write(&tok, "secret").unwrap();
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o644)).unwrap();
        let cfg = dir.join("c.toml");
        std::fs::write(&cfg, format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"{}\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n",
            tok.display()
        )).unwrap();
        match load(&cfg) {
            Err(ConfigError::InsecureFile { mode, .. }) => assert_eq!(mode, 0o644),
            other => panic!("expected insecure token_file, got {other:?}"),
        }
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&cfg).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_key_mode_refused_on_load() {
        let dir = std::env::temp_dir().join(format!("lu-cfg-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("k.pem");
        std::fs::write(&key, "k").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let cfg = dir.join("c.toml");
        std::fs::write(&cfg, format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[tls]\ncert=\"/c\"\nkey=\"{}\"\n",
            key.display()
        )).unwrap();
        assert!(matches!(load(&cfg), Err(ConfigError::InsecureFile { .. })));
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&cfg).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_size_cap() {
        let dir = std::env::temp_dir().join(format!("lu-cfg-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.toml");
        std::fs::write(&path, "#".repeat(MAX_CONFIG_BYTES as usize + 1)).unwrap();
        assert!(matches!(load(&path), Err(ConfigError::TooLarge { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_table_needs_both_paths() {
        assert!(Config::parse(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[tls]\ncert=\"/c\"\n"
        )
        .is_err());
        expect_table(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[tls]\ncert=\"/c\"\nkey=\"\"\n",
            "[tls]",
        );
    }

    #[test]
    fn mirror_credential_names_accept_repo_urls_and_normalized_ids() {
        let base = controller_only();
        let mut text = format!("{base}\n[mirror.credentials]\n");
        for name in [
            "app",
            "https://example.net/team/app.git",
            "example.net/team/app",
            "ssh://example.net/team/app.git",
        ] {
            text.push_str(&format!(
                "[[mirror.credentials.repos]]\nname = \"{name}\"\ntoken_file = \"/tmp/lu-token\"\n"
            ));
        }
        Config::parse(&text).unwrap();

        let bad = format!(
            "{base}\n[mirror.credentials]\n[[mirror.credentials.repos]]\nname = \"bad name\"\ntoken_file = \"/tmp/lu-token\"\n"
        );
        let (table, reason) = invalid(Config::parse(&bad).unwrap_err());
        assert!(
            table.starts_with("[mirror.credentials.repos] #0"),
            "{table}"
        );
        assert!(reason.contains("A-Z a-z 0-9"));
    }
}

#[cfg(test)]
mod operator_tests {
    use super::tests::controller_only;
    use super::*;

    #[test]
    fn an_operator_file_expands_home_and_checks_the_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operator.toml");
        std::fs::write(
            &path,
            "url = \"https://127.0.0.1:7600\"\ntoken_file = \"~/t\"\nca_file = \"/ca\"\n",
        )
        .unwrap();
        let config = load_operator(&path).unwrap();
        assert!(
            !config.token_file.starts_with('~')
                || std::env::var("HOME").unwrap_or_default().is_empty()
        );
        assert_eq!(config.ca_file.as_deref(), Some("/ca"));
        std::fs::write(&path, "url = \"ftp://x\"\ntoken_file = \"/t\"\n").unwrap();
        assert!(load_operator(&path).is_err());
        std::fs::write(
            &path,
            "url = \"https://x\"\ntoken_file = \"/t\"\nextra = 1\n",
        )
        .unwrap();
        assert!(load_operator(&path).is_err());
    }

    #[test]
    fn operator_roles_expiry_and_audit_requirement() {
        let base = controller_only();
        // Defaults keep existing configurations valid.
        assert!(Config::parse(&base).is_ok());
        let digest = "7f".repeat(32);
        let with_operator = |extra: &str, audit: bool| {
            let mut text = base.clone();
            text.push_str(&format!(
                "\n[[controller.operators]]\nname = \"ui\"\ntoken_sha256 = \"{digest}\"\n{extra}\n"
            ));
            if audit {
                text.push_str("\n[audit]\npath = \"/var/log/lu/audit.jsonl\"\n");
            }
            text
        };
        // Viewer and restrict_fields need an audit record.
        assert!(Config::parse(&with_operator("role = \"viewer\"", false)).is_err());
        assert!(Config::parse(&with_operator("restrict_fields = true", false)).is_err());
        let cfg = Config::parse(&with_operator(
            "role = \"viewer\"\nrestrict_fields = true",
            true,
        ))
        .unwrap();
        let controller = cfg.controller.unwrap();
        let op = controller
            .operators
            .iter()
            .find(|o| o.name == "ui")
            .unwrap();
        assert_eq!(op.role, OperatorRole::Viewer);
        assert!(op.restrict_fields);
        // Expiry is a strict UTC timestamp.
        assert!(Config::parse(&with_operator(
            "expires_at = \"2030-01-31T12:00:00Z\"",
            false
        ))
        .is_ok());
        for bad in [
            "2030-01-31",
            "2030-02-30T00:00:00Z",
            "2030-01-31T12:00:00+01:00",
        ] {
            let text = with_operator(&format!("expires_at = \"{bad}\""), false);
            assert!(Config::parse(&text).is_err(), "{bad}");
        }
        assert!(Config::parse(&with_operator("role = \"root\"", true)).is_err());
    }

    #[test]
    fn utc_timestamps_parse_to_unix_seconds() {
        assert_eq!(parse_utc_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_utc_timestamp("2000-03-01T00:00:00Z"),
            Some(951_868_800)
        );
        assert_eq!(
            parse_utc_timestamp("2024-02-29T23:59:59Z"),
            Some(1_709_251_199)
        );
        assert_eq!(parse_utc_timestamp("2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_utc_timestamp("2030-13-01T00:00:00Z"), None);
        assert_eq!(parse_utc_timestamp("2030-01-01 00:00:00Z"), None);
    }
}
