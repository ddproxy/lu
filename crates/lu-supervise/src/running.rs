use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustix::process::{kill_process_group, Pid, Signal};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::command::{build, CommandSpec};
use crate::limits::Limits;
use crate::output::{OutputChunk, OutputState, Stream, MAX_CHUNK};

/// Output buffered between a pipe reader and a slow consumer. Small on purpose:
/// it bounds memory, and the reader waits when it is full.
const CHANNEL_CAPACITY: usize = 16;

/// How a supervised process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The process exited normally with this code.
    Exited(i32),
    /// The process was killed by this signal.
    Signalled(i32),
    /// The wall-clock timeout fired.
    WallTimeout,
    /// The process produced no output for the idle-output timeout.
    IdleTimeout,
    /// [`Running::stop`] was called and the group was ended.
    Stopped,
    /// More than [`Limits::max_output_bytes`] was produced.
    OutputLimit,
    /// The process could not be started (or reaped) at all.
    SpawnFailed(String),
}

/// Starts and supervises harness processes.
pub struct Supervisor;

impl Supervisor {
    /// Starts `spec` under `limits` in its own process group.
    ///
    /// A spawn failure is not an error here: it is reported by the returned
    /// [`Running::wait`] as [`Outcome::SpawnFailed`].
    pub fn spawn(spec: CommandSpec, limits: Limits) -> Running {
        let mut command = build(&spec);
        let (out_tx, out_rx) = mpsc::channel::<OutputChunk>(CHANNEL_CAPACITY);
        let control = Arc::new(Control {
            stop: Notify::new(),
            reaped: AtomicBool::new(false),
        });

        match command.spawn() {
            Ok(mut child) => {
                let pgid = child
                    .id()
                    .and_then(|id| i32::try_from(id).ok())
                    .and_then(Pid::from_raw);
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let (trip_tx, trip_rx) = mpsc::unbounded_channel();
                let (outcome_tx, outcome_rx) = oneshot::channel();
                match pgid {
                    Some(pgid) => {
                        let max_output_bytes = limits.max_output_bytes;
                        let job = Job {
                            child,
                            pgid,
                            limits,
                            control: Arc::clone(&control),
                            state: Arc::new(OutputState::new(max_output_bytes)),
                            out_tx,
                            trip_tx,
                            trip_rx,
                            outcome_tx,
                            stdout,
                            stderr,
                        };
                        tokio::spawn(supervise(job));
                        Running {
                            out_rx: Some(out_rx),
                            outcome_rx: Some(outcome_rx),
                            outcome: None,
                            control,
                            pgid: Some(pgid),
                            finished: false,
                        }
                    }
                    None => {
                        // No pid means no process group to signal; treat it as a failure.
                        drop(child);
                        let _ = outcome_tx
                            .send(Outcome::SpawnFailed("spawned child has no pid".to_owned()));
                        Running {
                            out_rx: Some(out_rx),
                            outcome_rx: Some(outcome_rx),
                            outcome: None,
                            control,
                            pgid: None,
                            finished: false,
                        }
                    }
                }
            }
            Err(error) => {
                drop(out_tx);
                Running {
                    out_rx: Some(out_rx),
                    outcome_rx: None,
                    outcome: Some(Outcome::SpawnFailed(error.to_string())),
                    control,
                    pgid: None,
                    finished: false,
                }
            }
        }
    }
}

/// A handle to a running child.
pub struct Running {
    out_rx: Option<mpsc::Receiver<OutputChunk>>,
    outcome_rx: Option<oneshot::Receiver<Outcome>>,
    outcome: Option<Outcome>,
    control: Arc<Control>,
    pgid: Option<Pid>,
    finished: bool,
}

impl Running {
    /// Takes the merged output stream.
    ///
    /// The first call returns the receiver; later calls return a closed one. If
    /// the receiver is dropped, output is discarded and supervision continues.
    pub fn output(&mut self) -> mpsc::Receiver<OutputChunk> {
        match self.out_rx.take() {
            Some(receiver) => receiver,
            None => closed_channel(),
        }
    }

    /// The process group id the harness runs in, or `None` when it never started.
    pub fn process_group(&self) -> Option<u32> {
        self.pgid
            .and_then(|pgid| u32::try_from(pgid.as_raw_pid()).ok())
    }

    /// Asks the supervisor to end the process group: terminate signal, grace
    /// period, then forced-stop signal.
    pub fn stop(&self) {
        if self.control.reaped.load(Ordering::SeqCst) {
            return;
        }
        self.control.stop.notify_one();
    }

    /// Reports whether the supervisor has reaped the direct child and finished
    /// its own final cleanup.
    #[cfg(test)]
    pub(crate) fn reaped(&self) -> bool {
        self.control.reaped.load(Ordering::SeqCst)
    }

    /// Waits for the run to end. The result is cached, so later calls return it again.
    ///
    /// Waiting without having taken [`Running::output`] discards the output: an unread stream
    /// would fill, stall the pipe readers, and the output and idle limits could never trip.
    pub async fn wait(&mut self) -> Outcome {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }
        drop(self.out_rx.take());
        let outcome = match self.outcome_rx.take() {
            Some(receiver) => receiver.await.unwrap_or_else(|_| {
                Outcome::SpawnFailed("supervisor ended without a result".to_owned())
            }),
            None => Outcome::SpawnFailed("supervisor result already taken".to_owned()),
        };
        self.finished = true;
        self.outcome = Some(outcome.clone());
        outcome
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Dropping without waiting must not leave the group running. The
        // supervisor normally does this, but it may already be gone. Once the
        // child has been reaped and the run is over, never signal the group:
        // the process id could have been recycled.
        if !self.finished && !self.control.reaped.load(Ordering::SeqCst) {
            if let Some(pgid) = self.pgid {
                let _ = kill_process_group(pgid, Signal::KILL);
            }
        }
    }
}

fn closed_channel() -> mpsc::Receiver<OutputChunk> {
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    rx
}

struct Control {
    stop: Notify,
    reaped: AtomicBool,
}

/// A reason to end the run that is not the process exiting on its own.
enum Trip {
    Idle,
    OutputLimit,
}

/// The owned state of one supervised run.
struct Job {
    child: Child,
    pgid: Pid,
    limits: Limits,
    control: Arc<Control>,
    state: Arc<OutputState>,
    out_tx: mpsc::Sender<OutputChunk>,
    trip_tx: mpsc::UnboundedSender<Trip>,
    trip_rx: mpsc::UnboundedReceiver<Trip>,
    outcome_tx: oneshot::Sender<Outcome>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

/// How the wait loop ended.
enum Finish {
    /// The direct child was reaped.
    Status(std::io::Result<ExitStatus>),
    /// Something else asked us to end the run.
    Reason(Outcome),
}

async fn supervise(job: Job) {
    let Job {
        mut child,
        pgid,
        limits,
        control,
        state,
        out_tx,
        trip_tx,
        mut trip_rx,
        outcome_tx,
        stdout,
        stderr,
    } = job;

    let mut readers = Vec::new();
    if let Some(stdout) = stdout {
        readers.push(tokio::spawn(pump(
            stdout,
            Stream::Stdout,
            out_tx.clone(),
            Arc::clone(&state),
            trip_tx.clone(),
        )));
    }
    if let Some(stderr) = stderr {
        readers.push(tokio::spawn(pump(
            stderr,
            Stream::Stderr,
            out_tx.clone(),
            Arc::clone(&state),
            trip_tx.clone(),
        )));
    }
    // Close the output channel once the readers finish.
    drop(out_tx);

    let idle_task = limits
        .idle_output_timeout
        .map(|idle| tokio::spawn(watch_idle(idle, Arc::clone(&state), trip_tx.clone())));
    // Keep one trip sender alive so `trip_rx.recv()` never returns `None` while
    // a pipe can still close: a child that closes stdout is not a stop request.
    let _trip_keepalive = trip_tx;

    let wall = tokio::time::sleep(limits.wall_timeout);
    tokio::pin!(wall);

    let finish = tokio::select! {
        status = child.wait() => Finish::Status(status),
        () = &mut wall => Finish::Reason(Outcome::WallTimeout),
        () = control.stop.notified() => Finish::Reason(Outcome::Stopped),
        trip = trip_rx.recv() => Finish::Reason(match trip {
            Some(Trip::Idle) => Outcome::IdleTimeout,
            Some(Trip::OutputLimit) => Outcome::OutputLimit,
            None => Outcome::Stopped,
        }),
    };

    let outcome = match finish {
        Finish::Status(status) => {
            // The direct child is gone; end any descendant still in the group.
            let _ = kill_process_group(pgid, Signal::KILL);
            let outcome = status_to_outcome(status);
            // A group that is already empty and a recycled id is the one residual
            // window; it is accepted because there is no group state to prove emptiness.
            control.reaped.store(true, Ordering::SeqCst);
            outcome
        }
        Finish::Reason(reason) => {
            let _ = kill_process_group(pgid, Signal::TERM);
            if tokio::time::timeout(limits.grace, child.wait())
                .await
                .is_err()
            {
                let _ = kill_process_group(pgid, Signal::KILL);
                let _ = child.wait().await;
            }
            // Descendants that ignored the terminate signal are ended now.
            let _ = kill_process_group(pgid, Signal::KILL);
            // A group that is already empty and a recycled id is the one residual
            // window; it is accepted because there is no group state to prove emptiness.
            control.reaped.store(true, Ordering::SeqCst);
            reason
        }
    };

    for reader in readers {
        reader.abort();
        let _ = reader.await;
    }
    if let Some(idle) = idle_task {
        idle.abort();
        let _ = idle.await;
    }
    let _ = outcome_tx.send(outcome);
}

/// Reads one pipe, forwarding chunks, and trips the output limit when exceeded.
async fn pump<R>(
    mut reader: R,
    stream: Stream,
    tx: mpsc::Sender<OutputChunk>,
    state: Arc<OutputState>,
    trip: mpsc::UnboundedSender<Trip>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0u8; MAX_CHUNK];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if state.record(read) > state.limit {
            let _ = trip.send(Trip::OutputLimit);
            break;
        }
        if state.discarding.load(Ordering::Relaxed) {
            continue;
        }
        let Some(bytes) = buffer.get(..read) else {
            break;
        };
        let chunk = OutputChunk {
            stream,
            bytes: bytes.to_vec(),
        };
        // A full channel blocks the read, so the child is not ended for a slow
        // consumer, but memory stays bounded.
        if tx.send(chunk).await.is_err() {
            state.discarding.store(true, Ordering::Relaxed);
        }
    }
}

/// Trips the idle-output timeout when no output arrives for `idle`.
async fn watch_idle(idle: Duration, state: Arc<OutputState>, trip: mpsc::UnboundedSender<Trip>) {
    loop {
        let deadline = state.last_output() + idle;
        let wait = deadline.saturating_duration_since(std::time::Instant::now());
        if wait.is_zero() {
            let _ = trip.send(Trip::Idle);
            return;
        }
        tokio::time::sleep(wait).await;
    }
}

fn status_to_outcome(status: std::io::Result<ExitStatus>) -> Outcome {
    match status {
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => Outcome::Exited(code),
            (None, Some(signal)) => Outcome::Signalled(signal),
            (None, None) => Outcome::Exited(-1),
        },
        Err(error) => Outcome::SpawnFailed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Supervisor;
    use crate::{CommandSpec, Limits};

    fn limits() -> Limits {
        Limits {
            wall_timeout: Duration::from_secs(30),
            idle_output_timeout: None,
            max_output_bytes: 64 * 1024 * 1024,
            grace: Duration::from_millis(200),
        }
    }

    fn sh(script: &str) -> CommandSpec {
        CommandSpec::new("sh").arg("-c").arg(script)
    }

    #[tokio::test]
    async fn a_run_that_ends_by_itself_is_reaped_before_drop_without_wait() {
        let run = Supervisor::spawn(sh("exit 0"), limits());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !run.reaped() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("run should be reaped");
        assert!(run.reaped());
        drop(run);
    }

    #[tokio::test]
    async fn a_run_dropped_while_still_running_is_not_reaped() {
        let run = Supervisor::spawn(sh("sleep 300"), limits());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!run.reaped());
        drop(run);
    }
}
