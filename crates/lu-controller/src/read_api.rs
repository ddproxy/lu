//! Read-side helpers for operator GET routes: the per-operator read budget, strong ETags, the
//! compact job summary, job list filters and the bounded job scan that serves them.

use crate::auth::authenticate_operator;
use crate::error::ApiError;
use crate::routes::operator::{query_u64, seconds};
use crate::routes::{failure_class_str, store_err};
use crate::scrub::Scrubber;
use crate::state::AppState;
use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use lu_auth::sha256_hex;
use lu_proto::{normalize_repository, JobId, JobState, NodeName};
use lu_serve::PeerAddr;
use lu_store::JobRecord;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Characters of the first task line shown as `title` in `view=summary`.
pub const SUMMARY_TITLE_CHARS: usize = 80;
/// Rows fetched per store call while a job filter is applied in memory.
pub const JOB_SCAN_BATCH: usize = 200;
/// Rows one filtered list request may examine before it returns a cursor to continue from.
pub const JOB_SCAN_MAX_ROWS: usize = 10_000;

/// Authenticates an operator for a GET route and spends one unit of that operator's read budget.
/// The budget is checked after authentication so anonymous or wrong-token probes cannot use it up.
pub fn authenticate_read(
    headers: &HeaderMap,
    peer: PeerAddr,
    state: &AppState,
) -> Result<String, ApiError> {
    let operator = authenticate_operator(headers, peer, &state.auth, &state.throttle)?;
    state
        .read_limiter
        .allow(&format!("read:{operator}"), state.read_per_minute)
        .map_err(ApiError::rate_limited)?;
    Ok(operator)
}

/// A strong validator: the first 128 bits of the SHA-256 of the exact response body.
pub fn etag_for(body: &str) -> String {
    format!("\"{}\"", &sha256_hex(body)[..32])
}

/// Weak comparison per RFC 9110 for `If-None-Match`, which also accepts `*`.
fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag)
}

/// Serializes `value` once and answers with its ETag, or with 304 and no body when the client
/// already holds that representation.
pub fn json_with_etag<T: Serialize>(headers: &HeaderMap, value: &T) -> Response {
    let Ok(body) = serde_json::to_string(value) else {
        return ApiError::internal().into_response();
    };
    let etag = etag_for(&body);
    if if_none_match_hits(headers, &etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag.as_str())
            .body(Body::empty())
            .expect("valid 304 response headers");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ETAG, etag.as_str())
        .body(Body::from(body))
        .expect("valid json response headers")
}

/// The job list body. `next` is the id to pass as `after`, or null when nothing was examined.
#[derive(Serialize)]
pub struct JobsPage<T: Serialize> {
    pub jobs: T,
    pub next: Option<String>,
}

/// The repository as `owner/name`, from the normalised form. Credentials, query, fragment and the
/// host are dropped here, so nothing outside the path can reach a summary.
pub fn repo_slug(raw: &str) -> Option<String> {
    let normalised = normalize_repository(raw)?;
    let (_host, path) = normalised.split_once('/')?;
    (!path.is_empty()).then(|| path.to_owned())
}

/// The first line of the task, scrubbed, without control characters, cut to the title length.
pub fn title_of(task: &str, scrubber: &Scrubber) -> String {
    let first = task.lines().next().unwrap_or("");
    let scrubbed =
        String::from_utf8_lossy(&scrubber.scrub(first.as_bytes(), false).data).into_owned();
    scrubbed
        .chars()
        .filter(|c| !c.is_control())
        .take(SUMMARY_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A compact job row for `view=summary`. Built from named fields only: task text, context and the
/// rest of the spec never enter this object.
pub fn job_summary(r: &JobRecord, scrubber: &Scrubber) -> Value {
    json!({
        "id": r.id,
        "state": r.state,
        "node": r.node,
        "attempt": r.attempt,
        "created_at": seconds(r.created_at),
        "updated_at": seconds(r.updated_at),
        "started_at": r.started_at.map(seconds),
        "finished_at": r.finished_at.map(seconds),
        "repo": repo_slug(&r.spec.repo),
        "origin": r.spec.origin,
        "priority": r.spec.priority,
        "group": r.spec.group,
        "provider": r.assigned_provider,
        "model": r.assigned_model,
        "title": title_of(&r.spec.task, scrubber),
        "failure_class": r.failure_class.map(failure_class_str),
    })
}

/// Which body shape a job list or single-job read returns.
pub fn wants_summary(q: &HashMap<String, String>) -> Result<bool, ApiError> {
    match q.get("view").map(String::as_str) {
        None | Some("") | Some("full") => Ok(false),
        Some("summary") => Ok(true),
        Some(_) => Err(ApiError::bad_request("invalid view")),
    }
}

/// Filters applied to `GET /v1/jobs`. An empty set of conditions matches every job.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct JobFilter {
    pub states: Vec<JobState>,
    pub node: Option<NodeName>,
    /// Normalised `owner/name`.
    pub repo: Option<String>,
    pub origin: Option<String>,
    pub updated_since: Option<i64>,
}

impl JobFilter {
    /// True when the store cannot apply every condition itself and rows must be checked here.
    /// A single state is passed to the store, so it does not count.
    fn in_memory(&self) -> bool {
        self.states.len() > 1
            || self.node.is_some()
            || self.repo.is_some()
            || self.origin.is_some()
            || self.updated_since.is_some()
    }

    pub fn matches(&self, r: &JobRecord) -> bool {
        (self.states.is_empty() || self.states.contains(&r.state))
            && self
                .node
                .as_ref()
                .is_none_or(|node| r.node.as_ref() == Some(node))
            && self
                .repo
                .as_deref()
                .is_none_or(|repo| repo_slug(&r.spec.repo).as_deref() == Some(repo))
            && self
                .origin
                .as_deref()
                .is_none_or(|origin| r.spec.origin.as_deref() == Some(origin))
            && self.updated_since.is_none_or(|since| r.updated_at >= since)
    }
}

/// Parses `state` (repeatable, comma lists allowed), `node`, `repo`, `origin` and
/// `updated_since`. Empty values mean the filter is absent, as before for `state`.
pub fn parse_job_filter(q: &HashMap<String, String>, uri: &Uri) -> Result<JobFilter, ApiError> {
    let mut states: Vec<JobState> = Vec::new();
    for raw in crate::routes::query_values(uri, "state") {
        for part in raw.split(',').filter(|part| !part.is_empty()) {
            let state = part
                .parse::<JobState>()
                .map_err(|_| ApiError::bad_request("invalid state"))?;
            if !states.contains(&state) {
                states.push(state);
            }
        }
    }
    let node = q
        .get("node")
        .filter(|value| !value.is_empty())
        .map(|raw| NodeName::parse(raw).map_err(|_| ApiError::bad_request("invalid node")))
        .transpose()?;
    let repo = q
        .get("repo")
        .filter(|value| !value.is_empty())
        .map(|raw| parse_repo_filter(raw))
        .transpose()?;
    let origin = q.get("origin").filter(|value| !value.is_empty()).cloned();
    let updated_since =
        query_u64(q, "updated_since")?.map(|value| i64::try_from(value).unwrap_or(i64::MAX));
    Ok(JobFilter {
        states,
        node,
        repo,
        origin,
        updated_since,
    })
}

/// Accepts `owner/name` (or a longer slash path), with a `.git` suffix dropped to match the stored
/// form. Anything with a host, credentials, a query or other characters is refused.
fn parse_repo_filter(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.strip_suffix(".git").unwrap_or(raw);
    let segments: Vec<&str> = trimmed.split('/').collect();
    let valid = trimmed.len() <= 512
        && segments.len() >= 2
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        });
    if valid {
        Ok(trimmed.to_owned())
    } else {
        Err(ApiError::bad_request("invalid repo"))
    }
}

/// Walks jobs newest first from `after`, keeping those that match `filter`, until `limit` are
/// kept, the table ends or the row budget is spent. The returned cursor is the last row examined,
/// so a partial page resumes exactly where the scan stopped. `states` is the store-side filter.
pub async fn scan_jobs(
    state: &Arc<AppState>,
    states: Option<JobState>,
    filter: &JobFilter,
    limit: usize,
    after: Option<JobId>,
) -> Result<(Vec<JobRecord>, Option<JobId>), ApiError> {
    if limit == 0 {
        return Ok((Vec::new(), None));
    }
    let batch = if filter.in_memory() {
        JOB_SCAN_BATCH
    } else {
        limit
    };
    let mut kept = Vec::new();
    let mut cursor = after;
    let mut last_examined = None;
    let mut examined = 0usize;
    'scan: loop {
        let rows = tokio::task::spawn_blocking({
            let store = Arc::clone(&state.store);
            let cursor = cursor.clone();
            move || store.list_jobs(states, batch, cursor.as_ref())
        })
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(store_err)?;
        let fetched = rows.len();
        for rec in rows {
            examined += 1;
            cursor = Some(rec.id.clone());
            last_examined = Some(rec.id.clone());
            if filter.matches(&rec) {
                kept.push(rec);
                if kept.len() == limit {
                    break 'scan;
                }
            }
        }
        if fetched < batch || examined >= JOB_SCAN_MAX_ROWS {
            break;
        }
    }
    Ok((kept, last_examined))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_keeps_only_the_path_of_the_normalised_form() {
        assert_eq!(
            repo_slug("https://git.example.test/team/app.git").as_deref(),
            Some("team/app")
        );
        assert_eq!(
            repo_slug("https://user:hunter2@git.example.test/team/app.git?token=abc#frag")
                .as_deref(),
            Some("team/app")
        );
        assert_eq!(
            repo_slug("git@git.example.test:team/app.git").as_deref(),
            Some("team/app")
        );
        assert_eq!(repo_slug("/srv/local/repo"), None);
    }

    #[test]
    fn title_is_the_first_line_cut_to_the_limit_without_controls() {
        let scrubber = Scrubber::from_config(None).expect("default scrubber");
        let long = "x".repeat(200);
        let task = format!("first\u{7} line {long}\nsecond line");
        let title = title_of(&task, &scrubber);
        assert_eq!(title.chars().count(), SUMMARY_TITLE_CHARS);
        assert!(!title.contains('\u{7}'));
        assert!(!title.contains("second"));
        assert_eq!(title_of("", &scrubber), "");
    }

    #[test]
    fn etag_is_strong_quoted_and_stable() {
        let tag = etag_for("{\"a\":1}");
        assert_eq!(tag, etag_for("{\"a\":1}"));
        assert_ne!(tag, etag_for("{\"a\":2}"));
        assert!(tag.starts_with('"') && tag.ends_with('"') && tag.len() == 34);
    }

    #[test]
    fn if_none_match_accepts_lists_weak_tags_and_star() {
        let tag = etag_for("body");
        let headers = |value: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::IF_NONE_MATCH, value.parse().expect("header"));
            h
        };
        assert!(if_none_match_hits(&headers(&tag), &tag));
        assert!(if_none_match_hits(
            &headers(&format!("\"other\", W/{tag}")),
            &tag
        ));
        assert!(if_none_match_hits(&headers("*"), &tag));
        assert!(!if_none_match_hits(&headers("\"other\""), &tag));
    }

    #[test]
    fn repo_filter_accepts_paths_and_refuses_hosts_and_credentials() {
        assert_eq!(
            parse_repo_filter("team/app.git").ok().as_deref(),
            Some("team/app")
        );
        assert!(parse_repo_filter("team").is_err());
        assert!(parse_repo_filter("https://x/team/app").is_err());
        assert!(parse_repo_filter("user:pw@team/app").is_err());
        assert!(parse_repo_filter("team//app").is_err());
    }
}
