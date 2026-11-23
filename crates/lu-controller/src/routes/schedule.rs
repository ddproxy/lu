//! Schedule inspection, explanation and reload routes.
//!
//! Read routes need an ordinary operator token. Reload additionally needs the `schedule`
//! operator permission; a rejected reload never replaces the active schedule.

use crate::auth::authenticate_operator;
use crate::error::{ApiError, ErrorCode};
use crate::routes::{json_ok, json_value_ok, parse_query, store_err, with_version, MAX_JSON_BODY};
use crate::state::AppState;
use crate::time::now;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use lu_audit::{Data, Event, Field};
use lu_proto::JobId;
use lu_schedule::{
    Admissibility, AdmissionRequest, Budget, Calendar, CalendarState, Constraint, Schedule,
    WindowKind,
};
use lu_serve::PeerAddr;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// The group used for a scoped child's gates is the parent's, not the child's reset `group`.
async fn admission_group(state: &Arc<AppState>, job: &lu_store::JobRecord) -> Option<String> {
    let parent = job.parent_job_id.clone()?;
    let store = Arc::clone(&state.store);
    match tokio::task::spawn_blocking(move || store.get_job(&parent)).await {
        Ok(Ok(parent)) => parent.spec.group.clone(),
        _ => None,
    }
}

fn format_unix(at: i64) -> String {
    let civil = lu_schedule::CivilInstant::from_unix(at, lu_schedule::FixedOffset::UTC);
    format!("{}T{}Z", civil.date.as_str(), civil.time.as_str())
}

fn format_local(at: i64, offset: lu_schedule::FixedOffset) -> String {
    let civil = lu_schedule::CivilInstant::from_unix(at, offset);
    let suffix = if offset.is_utc() {
        "Z".to_owned()
    } else {
        offset.as_str()
    };
    format!("{}T{}{suffix}", civil.date.as_str(), civil.time.as_str())
}

fn parse_instant(text: &str) -> Result<i64, String> {
    if let Ok(at) = text.parse::<i64>() {
        return Ok(at);
    }
    lu_schedule::CivilInstant::parse(text, lu_schedule::FixedOffset::UTC)
        .map(|instant| instant.to_unix())
        .map_err(|error| error.reason().to_owned())
}

fn state_value(calendar: &Calendar, state: CalendarState, schedule: &Schedule) -> Value {
    json!({
        "available": state.available,
        "state": if state.available { "available" } else { "unavailable" },
        "since": state.since,
        "since_text": format_local(state.since, calendar.offset),
        "until": state.until,
        "until_text": format_local(state.until, calendar.offset),
        "rule": state.rule,
        "next_change": next_change(calendar, state.until, schedule),
    })
}

fn next_change(calendar: &Calendar, after: i64, schedule: &Schedule) -> Option<i64> {
    let mut budget = Budget::new(schedule.defaults.step_budget);
    calendar
        .next_change(after, schedule.defaults.horizon_secs, &mut budget)
        .ok()
        .flatten()
}

fn required_schedule(state: &AppState) -> Result<Schedule, ApiError> {
    state.schedule().ok_or_else(|| {
        ApiError::new(
            ErrorCode::NotFound,
            "scheduling is not configured on this controller",
        )
    })
}

/// `GET /v1/schedule`: current time, next boundary, and the state of every calendar and scope.
pub async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Extension<PeerAddr>,
    uri: axum::http::Uri,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let Some(schedule) = state.schedule() else {
        return with_version(json_value_ok(
            StatusCode::OK,
            json!({"configured": false, "calendars": [], "attachments": []}),
        ));
    };
    let q = parse_query(&uri);
    let at = match q.get("at").map(String::as_str) {
        Some(text) => match parse_instant(text) {
            Ok(at) => at,
            Err(error) => {
                return with_version(
                    ApiError::bad_request(format!("invalid at: {error}")).into_response(),
                )
            }
        },
        None => now().wall_unix,
    };
    let mut budget = Budget::new(schedule.defaults.step_budget);
    let calendars: Vec<Value> = schedule
        .calendars
        .iter()
        .map(|calendar| {
            let current = calendar
                .state_at(at, schedule.defaults.horizon_secs, &mut budget)
                .ok();
            json!({
                "name": calendar.name,
                "offset": calendar.offset.as_str(),
                "state": current.map(|s| state_value(calendar, s, &schedule)),
            })
        })
        .collect();
    let attachments: Vec<Value> = schedule
        .attachments
        .iter()
        .map(|attachment| {
            json!({
                "scope": attachment.scope.as_str(),
                "name": attachment.name,
                "calendars": attachment.calendars,
            })
        })
        .collect();
    let next = schedule.next_change(at);
    let body = json!({
        "configured": true,
        "now": at,
        "now_text": format_unix(at),
        "next_change": next,
        "next_change_in": next.map(|next| next.saturating_sub(at).max(0)),
        "next_change_text": next.map(format_unix),
        "defaults": schedule.defaults,
        "calendars": calendars,
        "attachments": attachments,
    });
    with_version(json_value_ok(StatusCode::OK, body))
}

fn calendar_detail(schedule: &Schedule, calendar: &Calendar, at: i64, days: i64) -> Value {
    let start = at.min(
        calendar
            .state_at(at, 1, &mut Budget::new(1))
            .map_or(at, |s| s.since),
    );
    let end = start.saturating_add(days.saturating_mul(86_400));
    let mut budget = Budget::new(schedule.defaults.step_budget);
    let available = calendar
        .intervals(start, end, WindowKind::Available, &mut budget)
        .unwrap_or_default();
    let unavailable = calendar
        .intervals(
            start,
            end,
            WindowKind::Unavailable,
            &mut Budget::new(budget.remaining()),
        )
        .unwrap_or_default();
    let expand = |intervals: &[lu_schedule::Interval]| -> Vec<Value> {
        intervals
            .iter()
            .map(|interval| {
                json!({
                    "start": interval.start,
                    "end": interval.end,
                    "start_text": format_local(interval.start, calendar.offset),
                    "end_text": format_local(interval.end, calendar.offset),
                })
            })
            .collect()
    };
    let mut value = serde_json::to_value(calendar).unwrap_or_else(|_| json!({}));
    let object = value
        .as_object_mut()
        .expect("calendar serializes to an object");
    object.insert("available".to_owned(), Value::Array(expand(&available)));
    object.insert("unavailable".to_owned(), Value::Array(expand(&unavailable)));
    object.insert("from".to_owned(), json!(start));
    object.insert("to".to_owned(), json!(end));
    value
}

/// `GET /v1/schedule/calendars` and `/v1/schedule/calendars/{name}`.
pub async fn list_calendars(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Extension<PeerAddr>,
    uri: axum::http::Uri,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let schedule = match required_schedule(&state) {
        Ok(schedule) => schedule,
        Err(error) => return with_version(error.into_response()),
    };
    let q = parse_query(&uri);
    let (at, days) = match parse_calendar_query(&q) {
        Ok(values) => values,
        Err(error) => return with_version(ApiError::bad_request(error).into_response()),
    };
    let calendars: Vec<Value> = schedule
        .calendars
        .iter()
        .map(|calendar| calendar_detail(&schedule, calendar, at, days))
        .collect();
    with_version(json_value_ok(
        StatusCode::OK,
        json!({"calendars": calendars, "from": at, "to": at.saturating_add(days * 86_400)}),
    ))
}

pub async fn get_calendar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Extension<PeerAddr>,
    Path(name): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let schedule = match required_schedule(&state) {
        Ok(schedule) => schedule,
        Err(error) => return with_version(error.into_response()),
    };
    let q = parse_query(&uri);
    let (at, days) = match parse_calendar_query(&q) {
        Ok(values) => values,
        Err(error) => return with_version(ApiError::bad_request(error).into_response()),
    };
    match schedule.calendar(&name) {
        Some(calendar) => with_version(json_ok(
            StatusCode::OK,
            &calendar_detail(&schedule, calendar, at, days),
        )),
        None => with_version(ApiError::not_found("unknown calendar").into_response()),
    }
}

fn parse_calendar_query(
    q: &std::collections::HashMap<String, String>,
) -> Result<(i64, i64), String> {
    let at = match q.get("from").map(String::as_str) {
        Some(text) => parse_instant(text).map_err(|error| format!("invalid from: {error}"))?,
        None => now().wall_unix,
    };
    let days = match q.get("days").map(String::as_str) {
        Some(text) => text
            .parse::<i64>()
            .map_err(|_| "invalid days".to_owned())?
            .clamp(1, 366),
        None => 7,
    };
    Ok((at, days))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplainBody {
    id: String,
    /// Evaluation time. Defaults to the controller's current wall clock.
    #[serde(default)]
    at: Option<i64>,
    /// Optional node override; without it every known node is evaluated.
    #[serde(default)]
    node: Option<String>,
}

fn request_for(
    now_at: i64,
    spec: &lu_proto::JobSpec,
    group: Option<String>,
    node: Option<String>,
    provider: Option<String>,
    model: Option<String>,
) -> AdmissionRequest {
    let job = spec.schedule.clone().unwrap_or_default();
    let mut request = AdmissionRequest::new(now_at, job, spec.timeout_secs as i64);
    request.eligible_since = now_at;
    request.group = group;
    request.node = node;
    request.provider = provider;
    request.model = model;
    request
}

fn admissibility_value(admissibility: Admissibility) -> Value {
    match admissibility {
        Admissibility::Fits { first_start } => {
            json!({"fits": true, "first_start": first_start, "first_start_text": format_unix(first_start)})
        }
        Admissibility::Never { reasons } => json!({"fits": false, "reasons": reasons}),
    }
}

/// `POST /v1/schedule/explain`: read-only per-candidate evaluation of one stored job.
pub async fn explain(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Extension<PeerAddr>,
    req: axum::extract::Request,
) -> Response {
    if authenticate_operator(&headers, peer.0, &state.auth, &state.throttle).is_err() {
        return with_version(ApiError::unauthorized().into_response());
    }
    let schedule = match required_schedule(&state) {
        Ok(schedule) => schedule,
        Err(error) => return with_version(error.into_response()),
    };
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_JSON_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return with_version(ApiError::too_large("request body too large").into_response())
        }
    };
    let body: ExplainBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => {
            return with_version(ApiError::unprocessable(error.to_string()).into_response())
        }
    };
    let id = match JobId::parse(&body.id) {
        Ok(id) => id,
        Err(_) => return with_version(ApiError::not_found("not found").into_response()),
    };
    let store = Arc::clone(&state.store);
    let rec = match tokio::task::spawn_blocking(move || store.get_job(&id)).await {
        Ok(Ok(rec)) => rec,
        Ok(Err(error)) => return with_version(store_err(error).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let at = body.at.unwrap_or_else(|| now().wall_unix);
    let group = admission_group(&state, &rec).await;
    let store = Arc::clone(&state.store);
    let nodes = match tokio::task::spawn_blocking(move || store.nodes(now(), state.lease_ms)).await
    {
        Ok(Ok(nodes)) => nodes,
        Ok(Err(error)) => return with_version(store_err(error).into_response()),
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let mut candidates: Vec<(Option<String>, Option<String>, Option<String>)> = Vec::new();
    if let Some(node) = &body.node {
        let record = nodes
            .iter()
            .find(|candidate| candidate.name.as_str() == node);
        candidates.extend(provider_candidates(&rec, record));
    } else {
        for node in &nodes {
            candidates.extend(provider_candidates(&rec, Some(node)));
        }
    }
    if candidates.is_empty() {
        candidates.push((None, None, None));
    }
    let truncated = candidates.len() > 64;
    candidates.truncate(64);
    let decisions: Vec<Value> = candidates
        .iter()
        .map(|(node, provider, model)| {
            let request = request_for(
                at,
                &rec.spec,
                group.clone(),
                node.clone(),
                provider.clone(),
                model.clone(),
            );
            let decision = match schedule.admit(&request) {
                Ok(admission) => serde_json::to_value(admission).unwrap_or(Value::Null),
                Err(error) => json!({"error": error.reason()}),
            };
            json!({"node": node, "provider": provider, "model": model, "decision": decision})
        })
        .collect();
    let first = request_for(at, &rec.spec, group, None, None, None);
    let admissibility = schedule
        .admissible(&first)
        .map(admissibility_value)
        .unwrap_or_else(|error| json!({"fits": false, "error": error.reason()}));
    with_version(json_value_ok(
        StatusCode::OK,
        json!({
            "job": rec.id.to_string(),
            "at": at,
            "at_text": format_unix(at),
            "spec": rec.spec.schedule,
            "decisions": decisions,
            "truncated": truncated,
            "admissibility": admissibility,
            "next_change": schedule.next_change(at),
        }),
    ))
}

fn provider_candidates(
    rec: &lu_store::JobRecord,
    node: Option<&lu_store::NodeRecord>,
) -> Vec<(Option<String>, Option<String>, Option<String>)> {
    let node_name = node.map(|node| node.name.as_str().to_owned());
    let providers: Vec<String> = match rec.spec.provider.as_deref() {
        Some(provider) if provider != lu_proto::ANY_PROVIDER => vec![provider.to_owned()],
        _ => node
            .map(|node| {
                node.heartbeat
                    .providers
                    .iter()
                    .map(|p| p.name.clone())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for provider in providers {
        let models: Vec<Option<String>> = match rec.spec.model.as_deref() {
            Some(model) => vec![Some(model.to_owned())],
            None => node
                .and_then(|node| {
                    node.heartbeat
                        .provider_models
                        .iter()
                        .find(|pair| pair.provider == provider)
                        .map(|pair| vec![Some(pair.model.clone())])
                })
                .unwrap_or_else(|| vec![None]),
        };
        for model in models {
            out.push((node_name.clone(), Some(provider.clone()), model));
        }
    }
    if out.is_empty() {
        out.push((node_name, None, None));
    }
    out
}

/// `POST /v1/schedule/reload`: revalidate the controller config and atomically swap the schedule.
pub async fn reload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: Extension<PeerAddr>,
) -> Response {
    let operator = match authenticate_operator(&headers, peer.0, &state.auth, &state.throttle) {
        Ok(operator) => operator,
        Err(error) => return with_version(error.into_response()),
    };
    if !state.auth.can_schedule(&operator) {
        return with_version(ApiError::forbidden("schedule permission required").into_response());
    }
    let path = match state.config_path() {
        Some(path) => path.to_path_buf(),
        None => {
            return with_version(
                ApiError::new(ErrorCode::Internal, "controller config path is unknown")
                    .into_response(),
            )
        }
    };
    let config = match tokio::task::spawn_blocking({
        let path = path.clone();
        move || lu_config::load_values(&path)
    })
    .await
    {
        Ok(Ok(config)) => config,
        Ok(Err(error)) => {
            state.record_audit(
                Event::ScheduleReload,
                None,
                None,
                &operator,
                Data::new().text(Field::Result, "rejected"),
            );
            return with_version(
                ApiError::unprocessable(format!("schedule reload rejected: {error}"))
                    .into_response(),
            );
        }
        Err(_) => return with_version(ApiError::internal().into_response()),
    };
    let schedule = config.schedule().cloned();
    let calendars = schedule.as_ref().map_or(0, |s| s.calendars.len()) as u64;
    let attachments = schedule.as_ref().map_or(0, |s| s.attachments.len()) as u64;
    state.set_schedule(schedule);
    state.waiters.notify_work();
    state.record_audit(
        Event::ScheduleReload,
        None,
        None,
        &operator,
        Data::new()
            .text(Field::Result, "applied")
            .unsigned(Field::Count, calendars)
            .unsigned(Field::Class, attachments),
    );
    with_version(json_value_ok(
        StatusCode::OK,
        json!({"applied": true, "calendars": calendars, "attachments": attachments}),
    ))
}

/// Used by the waiting-reason view to count budget refusals as a metric too.
pub fn note_blockers(state: &AppState, reasons: &[lu_schedule::BlockReason]) {
    for reason in reasons {
        state.record_schedule_wait(reason.constraint.as_str());
        if reason.constraint == Constraint::EvalBudget {
            state.note_eval_budget_exceeded();
        }
    }
}
