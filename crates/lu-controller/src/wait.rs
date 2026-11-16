use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const MAX_POLL_WAITERS: usize = 512;
const MAX_LOG_WAITERS: usize = 512;

#[derive(Debug, Default)]
pub struct WaitGroup {
    notify: Notify,
    _count: AtomicUsize,
}

impl WaitGroup {
    fn notify_waiters(&self) {
        self.notify.notify_waiters();
    }
}

#[derive(Debug)]
pub struct Waiters {
    /// General work changes: job creation, cancel, requeue, assignment-affecting changes.
    work: WaitGroup,
    poll_slots: AtomicUsize,
    log_slots: AtomicUsize,
    /// Per-job notifications for log/state changes.
    jobs: Mutex<HashMap<String, Arc<WaitGroup>>>,
    shutting_down: std::sync::atomic::AtomicBool,
}

impl Default for Waiters {
    fn default() -> Self {
        Self {
            work: WaitGroup {
                notify: Notify::new(),
                _count: AtomicUsize::new(0),
            },
            poll_slots: AtomicUsize::new(0),
            log_slots: AtomicUsize::new(0),
            jobs: Mutex::new(HashMap::new()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

pub struct PollPermit<'a> {
    waiters: &'a Waiters,
}
impl Drop for PollPermit<'_> {
    fn drop(&mut self) {
        self.waiters.poll_slots.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct LogPermit {
    waiters: Arc<Waiters>,
    job_id: String,
}
impl Drop for LogPermit {
    fn drop(&mut self) {
        self.waiters.log_slots.fetch_sub(1, Ordering::SeqCst);
        // keep per-job group; cheap enough and avoids races removing while others may look up.
        // count tracked implicitly by permit count cap; notifications are harmless.
        let _ = &self.job_id;
    }
}

impl Waiters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify_work(&self) {
        self.work.notify_waiters();
    }

    pub fn notify_job(&self, job_id: &str) {
        let g = self.job_group(job_id);
        g.notify_waiters();
        self.work.notify_waiters();
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.notify_work();
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for g in jobs.values() {
            g.notify_waiters();
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    fn job_group(&self, job_id: &str) -> Arc<WaitGroup> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.entry(job_id.to_owned())
            .or_insert_with(|| {
                Arc::new(WaitGroup {
                    notify: Notify::new(),
                    _count: AtomicUsize::new(0),
                })
            })
            .clone()
    }

    pub fn try_acquire_poll(&self) -> Option<PollPermit<'_>> {
        loop {
            let cur = self.poll_slots.load(Ordering::SeqCst);
            if cur >= MAX_POLL_WAITERS {
                return None;
            }
            if self
                .poll_slots
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(PollPermit { waiters: self });
            }
        }
    }

    pub fn try_acquire_log(self: &Arc<Self>, job_id: &str) -> Option<(LogPermit, Arc<WaitGroup>)> {
        loop {
            let cur = self.log_slots.load(Ordering::SeqCst);
            if cur >= MAX_LOG_WAITERS {
                return None;
            }
            if self
                .log_slots
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let g = self.job_group(job_id);
                return Some((
                    LogPermit {
                        waiters: Arc::clone(self),
                        job_id: job_id.to_owned(),
                    },
                    g,
                ));
            }
        }
    }

    /// Wait for general work notification or timeout. Caller must hold a poll permit.
    pub async fn wait_work(&self, timeout: std::time::Duration) -> bool {
        tokio::select! {
            _ = self.work.notify.notified() => true,
            _ = tokio::time::sleep(timeout) => false,
        }
    }

    /// Wait for a specific job notification or timeout.
    pub async fn wait_job(group: &WaitGroup, timeout: std::time::Duration) -> bool {
        tokio::select! {
            _ = group.notify.notified() => true,
            _ = tokio::time::sleep(timeout) => false,
        }
    }
}
