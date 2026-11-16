//! Lu config. Unknown keys are refused, the file is size-capped, every validation error
//! names its table, and secret files other users can read are refused.

mod error;
mod secret;

use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path};

use serde::Deserialize;

pub use error::ConfigError;
pub use secret::check_private_file;

/// A config is a few KiB. The cap stops a runaway or hostile file from being read whole.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub controller: Option<ControllerConfig>,
    pub agent: Option<AgentConfig>,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub service: ServiceConfig,
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
    #[serde(default)]
    pub agents: Vec<ControllerAgent>,
    #[serde(default)]
    pub operators: Vec<ControllerOperator>,
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
    #[serde(default)]
    pub cordoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerOperator {
    pub name: String,
    pub token_sha256: String,
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
    pub opencode_server: Option<String>,
    #[serde(default = "default_slots")]
    pub slots: u32,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    pub reiki_health_url: Option<String>,
    pub reiki_token_file: Option<String>,
}

fn default_slots() -> u32 {
    1
}
fn default_heartbeat_secs() -> u64 {
    15
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServiceConfig {
    #[serde(default = "default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
}

fn default_shutdown_timeout_secs() -> u64 {
    25
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            shutdown_timeout_secs: default_shutdown_timeout_secs(),
        }
    }
}

impl Config {
    /// Parses and validates config text. Does not touch the filesystem.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.controller.is_none() && self.agent.is_none() {
            return invalid(
                "[config]",
                "at least one of [controller] and [agent] must be present".to_owned(),
            );
        }
        self.validate_log()?;
        self.validate_service()?;
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

    fn validate_service(&self) -> Result<(), ConfigError> {
        if !(1..=300).contains(&self.service.shutdown_timeout_secs) {
            return invalid(
                "[service]",
                format!(
                    "shutdown_timeout_secs must be 1 to 300 (got {})",
                    self.service.shutdown_timeout_secs
                ),
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
        if !(1..=1).contains(&self.slots) {
            return invalid(
                "[agent]",
                format!("slots must be 1 to 1 (got {})", self.slots),
            );
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
        Ok(())
    }
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

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // strip brackets for IPv6 literal
    let h = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    h.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn parse_url_parts(url: &str) -> Option<(&str, &str, &str)> {
    // returns (scheme, host, rest_path_maybe) best-effort; rejects creds/query/fragment in validate
    let (scheme, rest) = url.split_once("://")?;
    // authority is up to first '/' or end
    let (authority, _path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    Some((scheme, authority, _path))
}

fn validate_controller_url(url: &str) -> Result<(), ConfigError> {
    if url.contains('@') {
        return invalid(
            "[agent]",
            "controller_url must not contain credentials".to_owned(),
        );
    }
    if url.contains('?') {
        return invalid(
            "[agent]",
            "controller_url must not contain a query".to_owned(),
        );
    }
    if url.contains('#') {
        return invalid(
            "[agent]",
            "controller_url must not contain a fragment".to_owned(),
        );
    }
    let (scheme, authority, _) = parse_url_parts(url).ok_or_else(|| ConfigError::Invalid {
        table: "[agent]".to_owned(),
        reason: format!("controller_url must be a valid URL (got {url:?})"),
    })?;
    if authority.is_empty() {
        return invalid("[agent]", "controller_url must include a host".to_owned());
    }
    let host = if authority.starts_with('[') {
        // [ipv6]:port or [ipv6]
        match authority.rfind(']') {
            Some(i) => &authority[..=i],
            None => {
                return invalid(
                    "[agent]",
                    "controller_url has an invalid IPv6 host".to_owned(),
                )
            }
        }
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    match scheme {
        "https" => Ok(()),
        "http" => {
            if is_loopback_host(host) {
                Ok(())
            } else {
                invalid(
                    "[agent]",
                    "controller_url may use http only on a loopback address; use https for non-loopback hosts"
                        .to_owned(),
                )
            }
        }
        _ => invalid(
            "[agent]",
            format!("controller_url must use https or http (got scheme {scheme:?})"),
        ),
    }
}

fn validate_loopback_http_url(table: &str, field: &str, url: &str) -> Result<(), ConfigError> {
    if url.contains('@') || url.contains('?') || url.contains('#') {
        return invalid(
            table,
            format!("{field} must not contain credentials, query, or fragment"),
        );
    }
    let (scheme, authority, _) = parse_url_parts(url).ok_or_else(|| ConfigError::Invalid {
        table: table.to_owned(),
        reason: format!("{field} must be a valid URL (got {url:?})"),
    })?;
    if scheme != "http" && scheme != "https" {
        return invalid(
            table,
            format!("{field} must use http or https (got scheme {scheme:?})"),
        );
    }
    let host = if authority.starts_with('[') {
        match authority.rfind(']') {
            Some(i) => &authority[..=i],
            None => return invalid(table, format!("{field} has an invalid IPv6 host")),
        }
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    if !is_loopback_host(host) {
        return invalid(
            table,
            format!("{field} must be a loopback address (got {url:?})"),
        );
    }
    Ok(())
}

/// Reads and validates a config file. Refuses files over [`MAX_CONFIG_BYTES`] and secret files
/// that group or others can read.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
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
    let config = Config::parse(&text)?;
    if let Some(tls) = &config.tls {
        check_private_file("[tls] key", Path::new(&tls.key))?;
    }
    if let Some(agent) = &config.agent {
        check_private_file("[agent] token_file", Path::new(&agent.token_file))?;
        if let Some(rt) = &agent.reiki_token_file {
            check_private_file("[agent] reiki_token_file", Path::new(rt))?;
        }
    }
    Ok(config)
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
        assert!(!c.agents[0].cordoned);
        let a = Config::parse(&agent_only()).unwrap().agent.unwrap();
        assert_eq!(a.slots, 1);
        assert_eq!(a.heartbeat_secs, 15);
        let cfg = Config::parse(&combined()).unwrap();
        assert_eq!(cfg.service.shutdown_timeout_secs, 25);
        assert_eq!(cfg.log.level, "info");
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
        for v in ["0", "2"] {
            expect_table(&base("slots", v), "[agent]");
        }
        for v in ["1", "301"] {
            expect_table(&base("heartbeat_secs", v), "[agent]");
        }
        assert!(Config::parse(&base("slots", "1")).is_ok());
        assert!(Config::parse(&base("heartbeat_secs", "2")).is_ok());
        assert!(Config::parse(&base("heartbeat_secs", "300")).is_ok());
    }

    #[test]
    fn service_range_names_table() {
        expect_table("[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[service]\nshutdown_timeout_secs=0\n", "[service]");
        expect_table("[controller]\nlisten=\"127.0.0.1:1\"\ndb=\"/db\"\nartifacts_dir=\"/a\"\n[service]\nshutdown_timeout_secs=301\n", "[service]");
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
