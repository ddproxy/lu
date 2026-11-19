//! Lu config. Unknown keys are refused, the file is size-capped, every validation error
//! names its table, and secret files other users can read are refused.

mod error;
mod secret;

use std::collections::HashSet;
use std::io::Read;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

pub use error::ConfigError;
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

/// Limits for operator-declared label probes; lu ships no built-in toolchain knowledge.
pub const MAX_DETECT_ENTRIES: usize = 16;
pub const MAX_DETECT_ARGV: usize = 8;
pub const MAX_DETECT_TIMEOUT_SECS: u64 = 30;
pub const MIN_DETECT_EVERY_SECS: u64 = 30;

const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub controller: Option<ControllerConfig>,
    pub agent: Option<AgentConfig>,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub log: LogConfig,
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
}

fn default_lease_secs() -> u64 {
    60
}
fn default_max_retries() -> u32 {
    1
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerOperator {
    pub name: String,
    pub token_sha256: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub controller_url: String,
    pub token_file: String,
    pub ca_file: Option<String>,
    pub work_dir: String,
    pub harness: String,
    /// How to obtain a job's base tree. Defaults to `auto`.
    #[serde(default)]
    pub base_source: BaseSource,
    pub opencode_server: Option<String>,
    /// A group (name or number) given write access to each job's working tree, for a harness
    /// that runs as another account, such as a container. Never covers `.git`.
    pub workspace_group: Option<String>,
    /// A private file holding the opencode server's password, passed to the harness as
    /// `OPENCODE_SERVER_PASSWORD`. The same rules as `token_file`: a regular file owned by the
    /// agent's account with no group or other access.
    pub opencode_password_file: Option<String>,
    /// A command that wraps the harness, such as `["podman", "exec", "-i", "--workdir", "{cwd}",
    /// "--env", "OPENCODE_SERVER_PASSWORD", "opencode-agent"]`, so it runs inside a container.
    /// `{cwd}` becomes the job's working tree. Empty runs the harness directly.
    #[serde(default)]
    pub harness_wrapper: Vec<String>,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    pub reiki_health_url: Option<String>,
    pub reiki_token_file: Option<String>,
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
}

fn default_heartbeat_secs() -> u64 {
    15
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
    /// Parses and validates config text. Does not touch the filesystem.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config = Self::parse_structure(text)?;
        config.validate()?;
        Ok(config)
    }

    fn parse_structure(text: &str) -> Result<Self, ConfigError> {
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
        self.validate_tls()?;
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
        if self.log.file.as_deref() == Some("") {
            return invalid(
                "[log]",
                "file must not be empty; leave it out to log without a file".to_owned(),
            );
        }
        Ok(())
    }

    fn validate_tls(&self) -> Result<(), ConfigError> {
        match &self.tls {
            Some(tls) if tls.cert.is_empty() || tls.key.is_empty() => {
                invalid("[tls]", "cert and key must both be set".to_owned())
            }
            _ => Ok(()),
        }
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
        if let Some(ca) = &self.ca_file {
            if ca.is_empty() {
                return invalid("[agent]", "ca_file must not be empty".to_owned());
            }
        }
        validate_abs_no_dotdot("[agent]", "work_dir", &self.work_dir)?;
        if self.harness.is_empty() {
            return invalid("[agent]", "harness must not be empty".to_owned());
        }
        if let Some(s) = &self.opencode_server {
            if s.is_empty() {
                return invalid("[agent]", "opencode_server must not be empty".to_owned());
            }
        }
        if let Some(file) = &self.opencode_password_file {
            if file.is_empty() {
                return invalid(
                    "[agent]",
                    "opencode_password_file must not be empty".to_owned(),
                );
            }
        }
        if self
            .harness_wrapper
            .iter()
            .any(|a| a.is_empty() || a.chars().any(char::is_control))
        {
            return invalid(
                "[agent]",
                "harness_wrapper entries must be non-empty text without control characters"
                    .to_owned(),
            );
        }
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
        if !(2..=300).contains(&self.heartbeat_secs) {
            return invalid(
                "[agent]",
                format!(
                    "heartbeat_secs must be 2 to 300 (got {})",
                    self.heartbeat_secs
                ),
            );
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
        match (&self.reiki_health_url, &self.reiki_token_file) {
            (Some(url), Some(file)) => {
                validate_loopback_http_url("[agent]", "reiki_health_url", url)?;
                if file.is_empty() {
                    return invalid("[agent]", "reiki_token_file must not be empty".to_owned());
                }
            }
            (Some(_), None) => {
                return invalid(
                    "[agent]",
                    "reiki_token_file is required when reiki_health_url is set".to_owned(),
                )
            }
            (None, Some(_)) => {
                return invalid(
                    "[agent]",
                    "reiki_token_file requires reiki_health_url".to_owned(),
                )
            }
            (None, None) => {}
        }
        if let Some(housekeeping) = &self.housekeeping {
            housekeeping.validate()?;
        }
        Ok(())
    }
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
            validate_scratch_glob(
                &format!("[agent.housekeeping] scratch_globs #{index}"),
                pattern,
            )?;
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

fn invalid(table: &str, reason: String) -> Result<(), ConfigError> {
    Err(ConfigError::Invalid {
        table: table.to_owned(),
        reason,
    })
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

fn validate_abs_no_dotdot(table: &str, field: &str, value: &str) -> Result<(), ConfigError> {
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
    Ok(config)
}

fn expand_home(value: &str, home: &str) -> String {
    match value.strip_prefix("~/") {
        Some(rest) if !home.is_empty() => format!("{home}/{rest}"),
        _ => value.to_owned(),
    }
}

fn read_config_text(path: &Path) -> Result<String, ConfigError> {
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
    let config = Config::parse(&read_config_text(path)?)?;
    if let Some(tls) = &config.tls {
        secret::read_private_file("[tls] key", Path::new(&tls.key), MAX_CONFIG_BYTES)?;
    }
    if let Some(agent) = &config.agent {
        secret::read_private_file(
            "[agent] token_file",
            Path::new(&agent.token_file),
            MAX_CONFIG_BYTES,
        )?;
        if let Some(rt) = &agent.reiki_token_file {
            secret::read_private_file("[agent] reiki_token_file", Path::new(rt), MAX_CONFIG_BYTES)?;
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
    Ok(config)
}

/// Parses a config file without checking values or reading the secret files it names. For
/// commands that only print a summary and do not use the secrets.
pub fn load_structure(path: &Path) -> Result<Config, ConfigError> {
    Config::parse_structure(&read_config_text(path)?)
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

    fn invalid(err: ConfigError) -> (String, String) {
        match err {
            ConfigError::Invalid { table, reason } => (table, reason),
            other => panic!("expected invalid, got {other}"),
        }
    }

    fn controller_only() -> String {
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
token_file = "/etc/lu/agent.token"
work_dir = "/srv/work/lu"
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
token_file = "/etc/lu/agent.token"
work_dir = "/srv/work/lu"
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
        assert_eq!(c.provider_cooldown_secs, 1800);
        assert_eq!(c.recovery_grace_secs, 30);
        assert_eq!(c.max_connections, 256);
        assert_eq!(c.max_connections_per_addr, 64);
        assert!(c.local_repo_roots.is_empty());
        let a = Config::parse(&agent_only()).unwrap().agent.unwrap();
        assert_eq!(a.heartbeat_secs, 15);
        assert_eq!(a.base_source, BaseSource::Auto);
        assert!(a.labels.is_empty());
        assert!(a.taints.is_empty());
        assert!(a.label_files.is_empty());
        let cfg = Config::parse(&combined()).unwrap();
        assert_eq!(cfg.log.level, "info");
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
        assert_eq!(config.min_age_hours, 6);
        assert_eq!(config.min_free_percent, 15);
        assert_eq!(config.max_remove_per_pass, 10);

        assert!(Config::parse(&format!(
            "{base}scratch_globs=[\"/srv/work/target-?\", \"/srv/work/build-*\", \"/tmp/x*\"]\n"
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

    fn expect_table(text: &str, table: &str) {
        let (t, r) = invalid(Config::parse(text).unwrap_err());
        assert_eq!(t, table, "reason: {r}");
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

    #[test]
    fn an_opencode_password_file_must_not_be_empty() {
        let base = |val: &str| {
            format!(
                "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\nopencode_password_file={val}\n"
            )
        };
        assert!(Config::parse(&base("\"/etc/lu/opencode.password\"")).is_ok());
        expect_table(&base("\"\""), "[agent]");
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
        assert!(Config::parse(&base("taints", r#"["homelab"]"#)).is_ok());
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
        assert!(r.contains("earlier agent"), "{r}");
        let dup_op = format!(
            "[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[[controller.operators]]\nname=\"o\"\ntoken_sha256=\"{HASH_A}\"\n[[controller.operators]]\nname=\"o\"\ntoken_sha256=\"{HASH_B}\"\n"
        );
        let (t, r) = invalid(Config::parse(&dup_op).unwrap_err());
        assert_eq!(t, "[[controller.operators]] #2 (o)");
        assert!(r.contains("earlier operator"), "{r}");
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
        assert!(t.starts_with("[[controller.operators]]"), "{t}");
        assert!(
            r.contains("both scopes") || r.contains("same as one used"),
            "{r}"
        );
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
    }

    #[test]
    fn url_host_parsing_strict_cases() {
        let agent = |url: &str| {
            format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"{url}\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n"
        )
        };
        let reiki = |url: &str| {
            format!(
            "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\nreiki_health_url=\"{url}\"\nreiki_token_file=\"/rt\"\n"
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
            expect_table(&reiki(url), "[agent]");
        }
        // accepted
        for url in [
            "http://[::ffff:127.0.0.1]:8080",
            "http://localhost:4100",
            "http://127.0.0.1:4100",
            "http://[::1]:4100",
        ] {
            assert!(Config::parse(&agent(url)).is_ok(), "agent accepted: {url}");
            assert!(Config::parse(&reiki(url)).is_ok(), "reiki accepted: {url}");
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
    fn reiki_rules() {
        let base = "[agent]\nname=\"a\"\ncontroller_url=\"https://example.net\"\ntoken_file=\"/t\"\nwork_dir=\"/w\"\nharness=\"opencode\"\n";
        expect_table(
            &format!("{base}reiki_health_url=\"http://127.0.0.1:4100/v1/health\"\n"),
            "[agent]",
        );
        assert!(Config::parse(&format!("{base}reiki_health_url=\"http://127.0.0.1:4100/v1/health\"\nreiki_token_file=\"/rt\"\n")).is_ok());
        expect_table(&format!("{base}reiki_health_url=\"http://example.net:4100/v1/health\"\nreiki_token_file=\"/rt\"\n"), "[agent]");
        expect_table(&format!("{base}reiki_token_file=\"/rt\"\n"), "[agent]");
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
        assert!(reason.contains("earlier repository"), "{reason}");
        expect_table(
            &base("name=\"mirror\"\nurl=\"https://example.net/a.git\"\nrefs=[]\n"),
            "[[controller.repos]] #1 (mirror)",
        );
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
}

#[cfg(test)]
mod operator_tests {
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
}
