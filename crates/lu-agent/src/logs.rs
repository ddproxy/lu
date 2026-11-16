//! Log streaming. The output reader never waits on the controller: chunks go through a bounded
//! queue and a separate uploader owns retries, offsets, and stale-epoch detection.

use std::sync::Arc;
use std::time::Duration;

use lu_client::{ApiError, Controller};
use lu_proto::{JobId, LogChunk, MAX_LOG_BYTES, MAX_LOG_CHUNK_BYTES};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::UploadFault;

/// Output kept for the completion summary.
pub struct Tail {
    bytes: Vec<u8>,
    cap: usize,
}

impl Tail {
    pub fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > self.cap {
            let excess = self.bytes.len() - self.cap;
            self.bytes.drain(..excess);
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    pub fn summary(&self) -> String {
        let text = self.text();
        let mut lines: Vec<&str> = text.lines().rev().take(12).collect();
        lines.reverse();
        crate::summarise(&lines.join("\n"))
    }
}

/// Splits arbitrary bytes into valid UTF-8 strings no larger than one log chunk. Invalid bytes
/// become replacement characters.
pub fn split_lossy(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut rest = text.as_ref();
    let mut out = Vec::new();
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_LOG_CHUNK_BYTES);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        out.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    out
}

/// What the uploader finally did. A fault is also sent on the live channel so a running job can
/// stop before the harness exits.
pub struct LogUpload {
    pub fault: Option<UploadFault>,
}

pub async fn upload_loop(
    controller: Arc<dyn Controller>,
    job_id: JobId,
    epoch: u64,
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut stop: oneshot::Receiver<()>,
    fault_tx: mpsc::Sender<UploadFault>,
) -> LogUpload {
    let mut offset = 0u64;
    let mut stopping = false;
    loop {
        let item = if stopping {
            rx.try_recv().ok()
        } else {
            tokio::select! {
                biased;
                _ = &mut stop => {
                    stopping = true;
                    continue;
                }
                item = rx.recv() => match item {
                    Some(item) => Some(item),
                    None => {
                        stopping = true;
                        continue;
                    }
                },
            }
        };
        let Some(item) = item else {
            break;
        };
        if item.is_empty() {
            continue;
        }
        match send_one(&controller, &job_id, epoch, offset, item).await {
            Ok(Sent { next, fault }) => {
                offset = next;
                if let Some(fault) = fault {
                    let _ = fault_tx.send(fault).await;
                    return LogUpload { fault: Some(fault) };
                }
            }
            Err(fault) => {
                let _ = fault_tx.send(fault).await;
                return LogUpload { fault: Some(fault) };
            }
        }
    }
    LogUpload { fault: None }
}

struct Sent {
    next: u64,
    fault: Option<UploadFault>,
}

/// Uploads one piece, resuming at the controller's stored length on a conflict. The wire chunk's
/// `seq` carries the byte offset because the controller's raw log route requires one.
async fn send_one(
    controller: &Arc<dyn Controller>,
    job_id: &JobId,
    epoch: u64,
    mut offset: u64,
    bytes: Vec<u8>,
) -> Result<Sent, UploadFault> {
    let mut data = String::from_utf8_lossy(&bytes).into_owned();
    let mut attempts = 0u32;
    loop {
        if offset >= MAX_LOG_BYTES || data.is_empty() {
            return Ok(Sent {
                next: offset,
                fault: None,
            });
        }
        let room = usize::try_from(MAX_LOG_BYTES - offset).unwrap_or(usize::MAX);
        if data.len() > room {
            let mut end = room;
            while end > 0 && !data.is_char_boundary(end) {
                end -= 1;
            }
            data.truncate(end);
        }
        if data.is_empty() {
            return Ok(Sent {
                next: offset,
                fault: None,
            });
        }
        let chunk = LogChunk {
            job_id: job_id.clone(),
            epoch,
            seq: offset,
            data: data.clone(),
        };
        match controller.append_log(&chunk).await {
            Ok(()) => {
                let len = u64::try_from(chunk.data.len()).unwrap_or(u64::MAX);
                return Ok(Sent {
                    next: offset.saturating_add(len),
                    fault: None,
                });
            }
            Err(ApiError::Conflict { length }) => {
                attempts = attempts.saturating_add(1);
                if length > offset {
                    let skip = length - offset;
                    let mut skip = usize::try_from(skip).unwrap_or(data.len());
                    if skip >= data.len() {
                        return Ok(Sent {
                            next: length,
                            fault: None,
                        });
                    }
                    while skip < data.len() && !data.is_char_boundary(skip) {
                        skip += 1;
                    }
                    data = data[skip..].to_owned();
                    offset = length;
                } else if length < offset {
                    // The controller lost part of the stream. Resume there rather than replay in
                    // the wrong place; the earlier prefix is already gone.
                    offset = length;
                }
                if attempts >= 8 {
                    debug!(job = %job_id, "giving up on a log chunk after repeated conflicts");
                    return Ok(Sent {
                        next: offset,
                        fault: None,
                    });
                }
            }
            Err(error) => match fault_of(&error) {
                Some(fault) => return Err(fault),
                None if error.is_transient() => {
                    attempts = attempts.saturating_add(1);
                    if attempts >= 6 {
                        warn!(
                            job = %job_id,
                            error = %error.message(),
                            "dropping a log chunk after repeated transient failures"
                        );
                        return Ok(Sent {
                            next: offset,
                            fault: None,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(200u64 << attempts.min(4))).await;
                }
                None => {
                    debug!(
                        job = %job_id,
                        error = %error.message(),
                        "dropping a log chunk the controller refused"
                    );
                    return Ok(Sent {
                        next: offset,
                        fault: None,
                    });
                }
            },
        }
    }
}

fn fault_of(error: &ApiError) -> Option<UploadFault> {
    match error {
        ApiError::StaleEpoch => Some(UploadFault::Stale),
        ApiError::Unauthorized => Some(UploadFault::Unauthorized),
        ApiError::Forbidden => Some(UploadFault::Forbidden),
        _ => None,
    }
}
