//! `lu top`: a live view of the nodes and recent jobs. The terminal loop runs on a blocking
//! thread; a SIGTERM sets the stop flag so the terminal guard restores the screen.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lu_client::Client;
use lu_proto::ProviderState;
use lu_tui::model::{JobRow, NodeRow, ProviderCell, Snapshot};

use crate::operator;

const REFRESH: Duration = Duration::from_secs(2);
const JOBS_SHOWN: u32 = 20;

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

/// The job's title: the first non-empty line of its task, without heading marks or a `Task:` lead.
fn job_title(task: &str) -> String {
    let line = task
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = line
        .strip_prefix("Task:")
        .or_else(|| line.strip_prefix("task:"))
        .unwrap_or(line)
        .trim();
    line.chars().filter(|c| !c.is_control()).collect()
}

/// Drops terminal escape sequences and keeps the last non-empty line, where a harness puts its result.
fn plain_summary(text: &str) -> String {
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
    out.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
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
                drained: node.cordoned,
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
            })
            .collect(),
        jobs: jobs
            .jobs
            .iter()
            .map(|job| JobRow {
                id: job.id.to_string(),
                title: job_title(&job.spec.task),
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
                    (None, None) => String::new(),
                },
            })
            .collect(),
    })
}

pub async fn run(config: &Path) -> Result<(), String> {
    let client = operator::connect(config)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    tokio::spawn(async move {
        lu_serve::shutdown_signal().await;
        flag.store(true, Ordering::Relaxed);
    });
    let (tx, rx) = mpsc::channel();
    let feeder_stop = Arc::clone(&stop);
    let feeder = tokio::spawn(async move {
        while !feeder_stop.load(Ordering::Relaxed) {
            if tx.send(snapshot(&client).await).is_err() {
                break;
            }
            tokio::time::sleep(REFRESH).await;
        }
    });
    let ui_stop = Arc::clone(&stop);
    let result = tokio::task::spawn_blocking(move || lu_tui::run::run_live(&rx, &ui_stop))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("terminal: {e}"));
    stop.store(true, Ordering::Relaxed);
    feeder.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::plain_summary;

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
            super::job_title("# Task: implement `lu top`\n\nbody"),
            "implement `lu top`"
        );
        assert_eq!(super::job_title("\n  plain words\nmore"), "plain words");
    }
}
