//! Append-only, hash-chained audit records of controller job events.
//!
//! Each line is a complete JSON object. `hash` is SHA-256 over the bytes of that line
//! without the `hash` field; `prev` repeats the previous line's `hash`. The first line's
//! `prev` is [`GENESIS_PREV`]. [`verify_bytes`] walks the chain and reports the first break.
//!
//! The `data` object is an allowlist of small scalar fields. Task text, prompts, artifact
//! content and tokens have no field they could be written to.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// `prev` on the first record.
pub const GENESIS_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Bounded queue between the request handlers and the writer task.
pub const QUEUE_CAPACITY: usize = 1024;

const MAX_DATA_KEYS: usize = 12;
const MAX_DATA_BYTES: usize = 512;
const MAX_SCALAR_BYTES: usize = 64;

/// The lifecycle events the controller records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Submitted,
    Leased,
    Started,
    Finished,
    Failed,
    Retried,
    /// A node was excluded from a job after a node-class infrastructure failure.
    NodeExcluded,
    Cancelled,
    CordonSet,
    CordonCleared,
    Drain,
    CollectApplied,
    /// One job-scoped action. `data.source` is always `child_job`, never free-form text.
    ScopeAction,
    /// A signed intake delivery outcome. `data.source` is the configured intake name.
    Intake,
    DraftApproved,
    DraftRejected,
    DraftExpired,
    /// An agent read its own node view. `data.source` is always `agent`.
    AgentSelf,
    /// One controller-side publish attempt. `data.target` is `snapshot` or `source_branch`,
    /// `data.result` is a fixed outcome code and `data.remote` is a configured remote name.
    Publish,
    /// High-confidence secret patterns found in added patch lines. `data.count` is the number of
    /// hits and `data.rules` names the rules; no matched secret text is recorded.
    PatchSecretDetected,
    /// A job or draft edit. `data.source` is the operator's role.
    JobEdited,
    /// A runtime group limit change. `data.count` is the new maximum, `data.source` the role.
    GroupLimit,
    /// A workspace clean request for a node. `data.source` is the operator's role.
    WorkspaceClean,
    /// A repository mirror sync. `data.remote` is the configured repository name.
    RepoSync,
    /// A queued or blocked job reached its start deadline and was ended.
    DeadlineMissed,
    /// A validated schedule was installed from the controller config.
    ScheduleReload,
    /// A dry run inspected a mutation without changing state. `data.outcome` is `dry_run`.
    MutationDryRun,
    /// A stored idempotency result was replayed. `data.outcome` is `replayed`.
    MutationReplay,
    /// A conditional mutation failed its precondition.
    MutationPreconditionFailed,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Leased => "leased",
            Self::Started => "started",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Retried => "retried",
            Self::NodeExcluded => "node_excluded",
            Self::Cancelled => "cancelled",
            Self::CordonSet => "cordon_set",
            Self::CordonCleared => "cordon_cleared",
            Self::Drain => "drain",
            Self::CollectApplied => "collect_applied",
            Self::ScopeAction => "scope_action",
            Self::Intake => "intake",
            Self::DraftApproved => "draft_approved",
            Self::DraftRejected => "draft_rejected",
            Self::DraftExpired => "draft_expired",
            Self::AgentSelf => "agent_self",
            Self::Publish => "publish",
            Self::PatchSecretDetected => "patch_secret_detected",
            Self::JobEdited => "job_edited",
            Self::GroupLimit => "group_limit",
            Self::WorkspaceClean => "workspace_clean",
            Self::RepoSync => "repo_sync",
            Self::DeadlineMissed => "deadline_missed",
            Self::ScheduleReload => "schedule_reload",
            Self::MutationDryRun => "mutation_dry_run",
            Self::MutationReplay => "mutation_replay",
            Self::MutationPreconditionFailed => "mutation_precondition_failed",
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The only keys allowed in a record's `data` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    ActorKind,
    Attempt,
    Class,
    Count,
    CordonMs,
    Epoch,
    ExitCode,
    ExcludedNode,
    Model,
    Mode,
    Node,
    Operation,
    Outcome,
    Principal,
    Provider,
    Reason,
    Remote,
    Result,
    Role,
    RetryAfterSecs,
    Rules,
    Source,
    State,
    Target,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Unsigned(u64),
    Signed(i64),
    Text(String),
    Bool(bool),
}

/// A small, secret-free data object. Text values are restricted to names and codes: a
/// free-form summary, prompt or artifact body cannot fit the character set or length cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Data {
    fields: BTreeMap<Field, Scalar>,
}

impl Data {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn unsigned(mut self, field: Field, value: u64) -> Self {
        self.put(field, Scalar::Unsigned(value));
        self
    }

    pub fn signed(mut self, field: Field, value: i64) -> Self {
        self.put(field, Scalar::Signed(value));
        self
    }

    pub fn bool(mut self, field: Field, value: bool) -> Self {
        self.put(field, Scalar::Bool(value));
        self
    }

    /// Adds a short name or code. Values outside the safe character set are dropped rather
    /// than recorded, so no job text can reach the file through a data field. URL-shaped values
    /// are stripped of userinfo, query and fragment before the character check.
    pub fn text(mut self, field: Field, value: &str) -> Self {
        let value = redact_url(value);
        if safe_text(value.as_ref()) {
            self.put(field, Scalar::Text(value.into_owned()));
        }
        self
    }

    fn put(&mut self, field: Field, value: Scalar) {
        if self.fields.len() >= MAX_DATA_KEYS && !self.fields.contains_key(&field) {
            return;
        }
        self.fields.insert(field, value);
        if serde_json::to_vec(&self.fields)
            .map(|bytes| bytes.len() > MAX_DATA_BYTES)
            .unwrap_or(true)
        {
            self.fields.remove(&field);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Removes userinfo, query and fragment from a URL-shaped value. Other values are untouched.
fn redact_url(value: &str) -> Cow<'_, str> {
    let Some((scheme, rest)) = value.split_once("://") else {
        return Cow::Borrowed(value);
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let path = &tail[..path_end];
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host == authority && path == tail {
        return Cow::Borrowed(value);
    }
    Cow::Owned(format!("{scheme}://{host}{path}"))
}

fn safe_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SCALAR_BYTES
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':' | b'+' | b'@')
        })
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SCALAR_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Actors are names or a job principal such as `job:ABC123`. The colon is the only extra byte
/// allowed here; it cannot carry task text, whitespace or punctuation used by log forging.
fn safe_actor(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SCALAR_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

/// One audit record as stored on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub seq: u64,
    pub time: String,
    pub event: Event,
    pub job: Option<String>,
    pub node: Option<String>,
    pub actor: String,
    pub data: BTreeMap<Field, Scalar>,
    pub prev: String,
    pub hash: String,
}

/// The serialized line with `hash` omitted. Keeping this as a struct fixes field order,
/// so the bytes covered by `hash` are unambiguous for any verifier.
#[derive(Serialize)]
struct WithoutHash<'a> {
    seq: u64,
    time: &'a str,
    event: Event,
    job: Option<&'a str>,
    node: Option<&'a str>,
    actor: &'a str,
    data: &'a BTreeMap<Field, Scalar>,
    prev: &'a str,
}

fn prefix(line: &Line) -> Vec<u8> {
    serde_json::to_vec(&WithoutHash {
        seq: line.seq,
        time: &line.time,
        event: line.event,
        job: line.job.as_deref(),
        node: line.node.as_deref(),
        actor: &line.actor,
        data: &line.data,
        prev: &line.prev,
    })
    .expect("serializing an audit line cannot fail")
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hash_prefix(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

/// Builds a canonical line and its bytes. `job`, `node` and `actor` are validated; callers
/// should let an invalid record be dropped (and the error counted) rather than fail a job.
fn make_line(seq: u64, prev: String, pending: Pending) -> Result<(Line, Vec<u8>), AuditError> {
    let Pending {
        time,
        event,
        job,
        node,
        actor,
        data,
    } = pending;
    if job.as_deref().is_some_and(|v| !safe_name(v))
        || node.as_deref().is_some_and(|v| !safe_name(v))
        || !safe_actor(&actor)
    {
        return Err(AuditError::InvalidRecord);
    }
    let mut line = Line {
        seq,
        time,
        event,
        job,
        node,
        actor,
        data: data.fields,
        prev,
        hash: String::new(),
    };
    let prefix = prefix(&line);
    line.hash = hash_prefix(&prefix);
    let mut bytes = prefix.clone();
    bytes.pop(); // insert `hash` just before the closing brace
    bytes.extend_from_slice(b",\"hash\":\"");
    bytes.extend_from_slice(line.hash.as_bytes());
    bytes.extend_from_slice(b"\"}\n");
    Ok((line, bytes))
}

#[derive(Debug)]
pub enum AuditError {
    Io(std::io::Error),
    Parse { line: usize, message: String },
    Break { line: usize, reason: &'static str },
    InvalidRecord,
    WrongHead { line: usize },
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "audit file I/O failed: {error}"),
            Self::Parse { line, message } => write!(f, "line {line}: invalid JSON ({message})"),
            Self::Break { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidRecord => write!(f, "invalid audit record fields"),
            Self::WrongHead { line } => {
                write!(
                    f,
                    "line {line}: head hash does not match the trusted anchor"
                )
            }
        }
    }
}

impl std::error::Error for AuditError {}

/// The result of a successful walk of the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub records: u64,
    pub head: String,
}

/// Walks a whole audit file. `expected_head`, when given, is a hash recorded elsewhere;
/// checking it also detects the removal of whole records from the end of the file.
pub fn verify_bytes(
    bytes: &[u8],
    expected_head: Option<&str>,
) -> Result<VerifyOutcome, AuditError> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(AuditError::Parse {
            line: 1,
            message: "unterminated last line".to_owned(),
        });
    }
    let mut prev = GENESIS_PREV.to_owned();
    let mut records = 0_u64;
    for (index, raw) in bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .enumerate()
    {
        let number = index + 1;
        let line: Line = serde_json::from_slice(raw).map_err(|error| AuditError::Parse {
            line: number,
            message: error.to_string(),
        })?;
        if line.seq != records + 1 {
            return Err(AuditError::Break {
                line: number,
                reason: "sequence gap or out-of-order record",
            });
        }
        if line.prev != prev {
            return Err(AuditError::Break {
                line: number,
                reason: "prev does not match the previous hash",
            });
        }
        if !line.time.ends_with('Z') || !line.hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AuditError::Break {
                line: number,
                reason: "malformed time or hash",
            });
        }
        let bytes_without_hash = prefix(&line);
        let mut canonical = bytes_without_hash.clone();
        canonical.pop();
        canonical.extend_from_slice(b",\"hash\":\"");
        canonical.extend_from_slice(line.hash.as_bytes());
        canonical.extend_from_slice(b"\"}");
        if canonical != raw {
            return Err(AuditError::Break {
                line: number,
                reason: "line bytes differ from their canonical form",
            });
        }
        if hash_prefix(&bytes_without_hash) != line.hash {
            return Err(AuditError::Break {
                line: number,
                reason: "hash mismatch",
            });
        }
        prev = line.hash;
        records += 1;
    }
    if let Some(expected) = expected_head {
        if expected != prev {
            return Err(AuditError::WrongHead {
                line: records.max(1) as usize,
            });
        }
    }
    Ok(VerifyOutcome {
        records,
        head: prev,
    })
}

pub fn verify_file(path: &Path, expected_head: Option<&str>) -> Result<VerifyOutcome, AuditError> {
    let bytes = std::fs::read(path).map_err(AuditError::Io)?;
    verify_bytes(&bytes, expected_head)
}

/// The last `count` lines, in file order, without their trailing newlines.
pub fn tail_bytes(bytes: &[u8], count: usize) -> Vec<String> {
    let mut lines: Vec<&[u8]> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    let start = lines.len().saturating_sub(count);
    lines.drain(..start);
    lines
        .into_iter()
        .filter_map(|line| String::from_utf8(line.to_vec()).ok())
        .collect()
}

/// The head hash and record count of an existing file, for restart and anchoring.
pub fn head_bytes(bytes: &[u8]) -> Result<VerifyOutcome, AuditError> {
    verify_bytes(bytes, None)
}

/// A non-blocking handle used by request handlers. Writes go through a bounded channel;
/// enqueue or write failures increment `errors` and are logged, never returned to a caller.
#[derive(Clone)]
pub struct Writer {
    tx: mpsc::Sender<Pending>,
    errors: Arc<AtomicU64>,
}

struct Pending {
    time: String,
    event: Event,
    job: Option<String>,
    node: Option<String>,
    actor: String,
    data: Data,
}

impl Writer {
    /// Opens (or creates) the file, verifies any existing chain, and starts the writer task.
    /// Dropping every `Writer` handle closes the channel; the returned task flushes and ends.
    pub async fn open(
        path: &Path,
        errors: Arc<AtomicU64>,
    ) -> Result<(Self, tokio::task::JoinHandle<()>), AuditError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(AuditError::Io)?;
            }
        }
        let (mut seq, mut prev) = match std::fs::read(path) {
            Ok(bytes) => {
                let outcome = head_bytes(&bytes)?;
                (outcome.records + 1, outcome.head)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (1, GENESIS_PREV.to_owned())
            }
            Err(error) => return Err(AuditError::Io(error)),
        };
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).await.map_err(AuditError::Io)?;
        let (tx, mut rx) = mpsc::channel::<Pending>(QUEUE_CAPACITY);
        let task_errors = Arc::clone(&errors);
        let task = tokio::spawn(async move {
            while let Some(pending) = rx.recv().await {
                let (line, bytes) = match make_line(seq, prev.clone(), pending) {
                    Ok(line) => line,
                    Err(error) => {
                        fail(&task_errors, &error.to_string());
                        continue;
                    }
                };
                if let Err(error) = file.write_all(&bytes).await {
                    fail(&task_errors, &format!("write failed: {error}"));
                    continue;
                }
                if let Err(error) = file.sync_data().await {
                    fail(&task_errors, &format!("sync failed: {error}"));
                    continue;
                }
                seq = line.seq.saturating_add(1);
                prev = line.hash;
            }
        });
        Ok((Self { tx, errors }, task))
    }

    /// Records an event if the queue has room. A full queue is a counted, logged failure.
    pub fn record(
        &self,
        event: Event,
        job: Option<&str>,
        node: Option<&str>,
        actor: &str,
        data: Data,
    ) {
        let pending = Pending {
            time: now_utc(),
            event,
            job: job.map(str::to_owned),
            node: node.map(str::to_owned),
            actor: actor.to_owned(),
            data,
        };
        if self.tx.try_send(pending).is_err() {
            fail(&self.errors, "audit queue full; record dropped");
        }
    }
}

fn fail(errors: &AtomicU64, reason: &str) {
    errors.fetch_add(1, Ordering::Relaxed);
    tracing::error!(%reason, "audit record not written");
}

/// UTC timestamp with millisecond precision, in the form `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn now_utc() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = (secs / 86_400) as i64;
    let within = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        within / 3600,
        (within / 60) % 60,
        within % 60,
        now.subsec_millis()
    )
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(seq: u64, prev: &str, event: Event, data: Data) -> (Line, Vec<u8>) {
        make_line(
            seq,
            prev.to_owned(),
            Pending {
                time: format!("2026-11-21T12:00:{seq:02}.000Z"),
                event,
                job: Some("job-1".to_owned()),
                node: Some("node-a".to_owned()),
                actor: "op".to_owned(),
                data,
            },
        )
        .expect("valid record")
    }

    #[test]
    fn canonical_line_hash_matches_bytes_without_hash() {
        let (record, bytes) = line(1, GENESIS_PREV, Event::Submitted, Data::new());
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        let cut = text.rfind(",\"hash\":\"").expect("hash field");
        // The line without its `hash` field keeps the object's closing brace.
        let without_hash = format!("{}}}", &text[..cut]);
        assert_eq!(record.hash, hash_prefix(without_hash.as_bytes()));
        assert!(text.ends_with("\"}\n"));
        assert_eq!(
            verify_bytes(&bytes, None).expect("valid chain"),
            VerifyOutcome {
                records: 1,
                head: record.hash
            }
        );
    }

    #[test]
    fn detects_edit_delete_insert_and_truncate() {
        let (first, a) = line(1, GENESIS_PREV, Event::Submitted, Data::new());
        let (_, b) = line(2, &first.hash, Event::Leased, Data::new());
        let (_, c) = line(3, &first.hash, Event::Started, Data::new());
        let mut good = Vec::new();
        good.extend_from_slice(&a);
        good.extend_from_slice(&b);

        let mut edited = good.clone();
        let text = String::from_utf8(edited.clone()).expect("utf8");
        edited = text
            .replace("\"event\":\"leased\"", "\"event\":\"started\"")
            .into_bytes();
        assert!(verify_bytes(&edited, None).is_err());

        let mut inserted = good.clone();
        inserted.extend_from_slice(&c);
        assert_eq!(
            verify_bytes(&inserted, None).unwrap_err().to_string(),
            "line 3: prev does not match the previous hash"
        );

        let mut deleted = Vec::new();
        deleted.extend_from_slice(&a);
        deleted.extend_from_slice(&c);
        assert_eq!(
            verify_bytes(&deleted, None).unwrap_err().to_string(),
            "line 2: sequence gap or out-of-order record"
        );

        let mut truncated = good.clone();
        truncated.pop();
        assert!(verify_bytes(&truncated, None).is_err());

        // Removing whole trailing records is only visible against an anchored head.
        let anchor = verify_bytes(&good, None).expect("valid chain").head;
        let mut shorter = Vec::new();
        shorter.extend_from_slice(&a);
        assert!(verify_bytes(&shorter, None).is_ok());
        assert_eq!(
            verify_bytes(&shorter, Some(&anchor))
                .unwrap_err()
                .to_string(),
            "line 1: head hash does not match the trusted anchor"
        );
    }

    #[test]
    fn job_principal_actor_is_written_and_verified() {
        let pending = Pending {
            time: "2026-11-21T12:00:01.000Z".to_owned(),
            event: Event::ScopeAction,
            job: Some("JOB123".to_owned()),
            node: Some("node-a".to_owned()),
            actor: "job:JOB123".to_owned(),
            data: Data::new().text(Field::Source, "child_job"),
        };
        let (record, bytes) = make_line(1, GENESIS_PREV.to_owned(), pending).expect("job actor");
        assert_eq!(record.actor, "job:JOB123");
        assert!(verify_bytes(&bytes, None).is_ok());
        assert!(!safe_actor("job:with space"));
        assert!(!safe_actor("job:with\nnewline"));
    }

    #[test]
    fn data_cannot_carry_task_text_or_secrets() {
        let data = Data::new()
            .text(Field::Provider, "provider-x")
            .text(Field::Model, "fix the bug with secret ABCDEFGH")
            .text(Field::Source, "line one\nline two")
            .unsigned(Field::Attempt, 2);
        let (record, bytes) = line(1, GENESIS_PREV, Event::Failed, data);
        assert_eq!(record.data.len(), 2);
        let text = String::from_utf8(bytes).expect("utf8");
        for secret in ["fix the bug", "ABCDEFGH", "line two", "prompt"] {
            assert!(
                !text.contains(secret),
                "task text leaked into the audit file"
            );
        }
        assert!(!text.contains("summary"));
        assert!(!text.contains("token"));
    }

    #[test]
    fn url_data_fields_are_redacted() {
        let user = ['r', 'e', 'a', 'd'].iter().collect::<String>();
        let pass = ['o', 'n', 'l', 'y'].iter().collect::<String>();
        let repo = format!("https://{user}:{pass}@example.net/repo.git?ref=main#part");
        let data = Data::new().text(Field::Source, &repo);
        let (_, bytes) = line(1, GENESIS_PREV, Event::Submitted, data);
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("https://example.net/repo.git"));
        assert!(!text.contains(&user));
        assert!(!text.contains(&pass));
        assert!(!text.contains("ref=main"));
        assert!(!text.contains("part"));
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_358), (2025, 9, 27));
        assert!(now_utc().ends_with('Z'));
    }

    #[tokio::test]
    async fn restart_continues_the_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let errors = Arc::new(AtomicU64::new(0));
        let (writer, task) = Writer::open(&path, Arc::clone(&errors))
            .await
            .expect("open writer");
        writer.record(Event::Submitted, Some("job-1"), None, "op", Data::new());
        writer.record(
            Event::Leased,
            Some("job-1"),
            Some("node-a"),
            "node-a",
            Data::new(),
        );
        drop(writer);
        task.await.expect("writer task");

        let first = verify_file(&path, None).expect("first chain");
        assert_eq!(first.records, 2);

        let (writer, task) = Writer::open(&path, Arc::clone(&errors))
            .await
            .expect("reopen writer");
        writer.record(
            Event::Leased,
            Some("job-1"),
            Some("node-a"),
            "node-a",
            Data::new(),
        );
        drop(writer);
        task.await.expect("writer task");

        let second = verify_file(&path, None).expect("continued chain");
        assert_eq!(second.records, 3);
        assert_ne!(second.head, first.head);
        assert_eq!(errors.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_full_queue_counts_an_error_without_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let errors = Arc::new(AtomicU64::new(0));
        let (writer, task) = Writer::open(&path, Arc::clone(&errors))
            .await
            .expect("open writer");
        for _ in 0..QUEUE_CAPACITY + 1 {
            writer.record(Event::Submitted, Some("job-1"), None, "op", Data::new());
        }
        assert_eq!(errors.load(Ordering::Relaxed), 1);
        drop(writer);
        task.await.expect("writer task");
    }
}
