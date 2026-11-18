//! `lu top`: a live view of the nodes and recent jobs. The terminal loop runs on a blocking
//! thread; a SIGTERM sets the stop flag so the terminal guard restores the screen.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lu_client::Client;
use lu_proto::{JobId, NodeName, ProviderState};
use lu_tui::model::{Dashboard, JobRow, NodeRow, ProviderCell, Request, Snapshot, Update};
use lu_tui::sanitize::printable;

use crate::operator;

const REFRESH: Duration = Duration::from_secs(2);
const JOBS_SHOWN: u32 = 20;
const LOG_TAIL_LINES: usize = 20;

#[derive(Debug, clap::Args)]
pub struct TopArgs {
    /// Fetch one snapshot, print the node and job tables, and exit.
    #[arg(long)]
    pub once: bool,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn provider_state(state: ProviderState) -> &'static str {
    match state {
        ProviderState::Ok => "ok",
        ProviderState::Cooling => "cooling",
        ProviderState::Disabled => "disabled",
        ProviderState::Unknown => "unknown",
    }
}

/// Drops terminal escape sequences, leaving line breaks and the rest of the text.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        // The controller stores a received ESC as '?', so "?[91m" is a colour code too.
        let escaped = c == '\u{1b}' || (c == '?' && chars.peek() == Some(&'['));
        if escaped {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Keeps the last non-empty line, where a harness puts its result.
fn plain_summary(text: &str) -> String {
    strip_ansi(text)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// The last lines of a job log, stripped of escapes and made printable for the pane.
fn log_tail(text: &str) -> String {
    let text = strip_ansi(text);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..]
        .iter()
        .map(|line| printable(line))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn snapshot(client: &Client) -> Result<Snapshot, String> {
    let now = unix_now();
    let nodes = client.nodes().await.map_err(|e| e.to_string())?;
    let jobs = client
        .list_jobs(None, Some(JOBS_SHOWN))
        .await
        .map_err(|e| e.to_string())?;
    Ok(Snapshot {
        nodes: nodes
            .nodes
            .iter()
            .map(|node| NodeRow {
                name: node.name.to_string(),
                harness: operator::harness_name(node.harness),
                drained: node.draining,
                seen_secs_ago: (node.last_seen > 0).then(|| now.saturating_sub(node.last_seen)),
                providers: node
                    .providers
                    .iter()
                    .map(|p| ProviderCell {
                        name: p.name.clone(),
                        state: provider_state(p.state).to_owned(),
                        soonest_secs: p.soonest_secs,
                    })
                    .collect(),
                labels: node.labels.clone(),
            })
            .collect(),
        jobs: jobs
            .jobs
            .iter()
            .map(|job| JobRow {
                id: job.id.to_string(),
                title: operator::job_title(&job.spec.task),
                state: operator::state_name(job.state).to_owned(),
                node: job.node.as_ref().map(ToString::to_string),
                attempt: job.attempt,
                age_secs: now.saturating_sub(job.created_at),
                model: job.spec.model.clone(),
                note: match (&job.failure_class, &job.summary) {
                    (Some(_), Some(summary)) => {
                        format!(
                            "{}: {}",
                            operator::failure_name(job.failure_class),
                            plain_summary(summary)
                        )
                    }
                    (Some(_), None) => operator::failure_name(job.failure_class),
                    (None, Some(summary)) => plain_summary(summary),
                    (None, None) => job.waiting_reason.clone().unwrap_or_default(),
                },
                after: job.after.iter().map(ToString::to_string).collect(),
            })
            .collect(),
    })
}

async fn fetch_log(client: &Client, job_id: &str) -> Result<String, String> {
    let id = JobId::parse(job_id).map_err(|error| error.to_string())?;
    let page = client
        .logs(&id, 0, None)
        .await
        .map_err(|error| error.to_string())?;
    Ok(log_tail(&page.data))
}

async fn print_once(client: &Client) -> Result<(), String> {
    let snapshot = snapshot(client).await?;
    let mut dashboard = Dashboard::new();
    let now = Instant::now();
    dashboard.apply(Ok(snapshot), now);
    println!("{}", dashboard.view_sized(now, 160).table_text());
    Ok(())
}

async fn print_json(client: &Client) -> Result<(), String> {
    let nodes = client.nodes().await.map_err(|error| error.to_string())?;
    let jobs = client
        .list_jobs(None, Some(JOBS_SHOWN))
        .await
        .map_err(|error| error.to_string())?;
    let body = serde_json::json!({ "nodes": nodes.nodes, "jobs": jobs.jobs });
    let text = serde_json::to_string(&body).map_err(|error| error.to_string())?;
    println!("{text}");
    Ok(())
}

pub async fn run(config: &Path, args: TopArgs, json: bool) -> Result<(), String> {
    if args.once && json {
        return Err("--once and --json are mutually exclusive".to_owned());
    }
    let client = operator::connect(config)?;
    if json {
        return print_json(&client).await;
    }
    if args.once {
        return print_once(&client).await;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    tokio::spawn(async move {
        lu_serve::shutdown_signal().await;
        flag.store(true, Ordering::Relaxed);
    });
    let (tx, rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let feeder_stop = Arc::clone(&stop);
    let feeder_client = client.clone();
    let feeder = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REFRESH);
        while !feeder_stop.load(Ordering::Relaxed) {
            tokio::select! {
                _ = ticker.tick() => {
                    if tx.send(snapshot(&feeder_client).await).is_err() {
                        break;
                    }
                }
                request = request_rx.recv() => {
                    match request {
                        Some(Request::Log { job_id }) => {
                            let result = fetch_log(&feeder_client, &job_id).await;
                            if update_tx.send(Update::Log { job_id, result }).is_err() {
                                break;
                            }
                        }
                        Some(Request::Drain { node, drain }) => {
                            let parsed = NodeName::parse(&node);
                            let result = match parsed {
                                Ok(name) => feeder_client
                                    .set_drain(&name, drain)
                                    .await
                                    .map(|_| ())
                                    .map_err(|error| error.to_string()),
                                Err(error) => Err(error.to_string()),
                            };
                            let update = Update::Drain { node, drain, result };
                            if update_tx.send(update).is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });
    let ui_stop = Arc::clone(&stop);
    let request = move |request: Request| {
        let _ = request_tx.send(request);
    };
    let result = tokio::task::spawn_blocking(move || {
        lu_tui::run::run_live(&rx, &update_rx, &request, &ui_stop)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("terminal: {e}"));
    stop.store(true, Ordering::Relaxed);
    feeder.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::{log_tail, plain_summary};
    use crate::operator::job_title;

    #[test]
    fn escapes_are_dropped_and_the_last_line_kept() {
        assert_eq!(
            plain_summary("\u{1b}[0m> build\n\u{1b}[91mok\u{1b}[0m\n"),
            "ok"
        );
        assert_eq!(
            plain_summary("?[91m?[1mError: ?[0mFile not found"),
            "Error: File not found"
        );
        assert_eq!(plain_summary(""), "");
        assert_eq!(
            job_title("# Task: implement `lu top`\n\nbody"),
            "implement `lu top`"
        );
        assert_eq!(job_title("\n  plain words\nmore"), "plain words");
    }

    #[test]
    fn a_log_tail_is_stripped_printed_and_limited_to_twenty_lines() {
        let lines = (1..=25)
            .map(|n| format!("\u{1b}[0mline {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = log_tail(&lines);
        assert_eq!(tail.lines().count(), 20);
        assert!(tail.starts_with("line 6"), "{tail}");
        assert!(tail.ends_with("line 25"), "{tail}");
        assert!(!tail.contains('\u{1b}'));
    }
}
