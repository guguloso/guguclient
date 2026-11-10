//! Cadence-ladder poller. Port of the Electron `Poller` class.
//!
//! A ladder is a list of tiers (`interval`, `duration`). After `trigger()`
//! the poller walks down the ladder as time passes, so it polls fast right
//! after user activity and slows down when idle. Failures back off
//! (1s, 2s, 5s, 15s, 30s, capped at 60s) without dropping the cadence.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::async_runtime::JoinHandle;
use tokio::sync::Notify;

use crate::error::AppResult;

/// `durationMs: Number.POSITIVE_INFINITY`.
pub const FOREVER: u64 = u64::MAX;

#[derive(Debug, Clone, Copy)]
pub struct Tier {
    pub interval_ms: u64,
    pub duration_ms: u64,
}

pub type Ladder = &'static [Tier];

const ERROR_BACKOFF_CAP_MS: u64 = 60_000;
const ERROR_BACKOFF_STEPS_MS: [u64; 5] = [1_000, 2_000, 5_000, 15_000, 30_000];

/// The work a poller drives. `run` covers both the fetch and `onResult`.
pub trait PollJob: Send + Sync + 'static {
    fn should_run(&self) -> bool;
    fn run(&self) -> impl Future<Output = AppResult<()>> + Send;
}

struct Control {
    foreground: Ladder,
    background: Option<Ladder>,
    backgrounded: bool,
    triggered_at: Instant,
    consecutive_errors: u32,
}

pub struct Poller {
    name: &'static str,
    control: Mutex<Control>,
    wake: Notify,
    task: Mutex<Option<JoinHandle<()>>>,
}

fn pick_tier(elapsed_ms: u64, ladder: Ladder) -> Tier {
    let mut accumulated = 0u64;
    for tier in ladder {
        accumulated = accumulated.saturating_add(tier.duration_ms);
        if elapsed_ms < accumulated {
            return *tier;
        }
    }
    *ladder.last().expect("ladder requires at least one tier")
}

fn error_backoff(attempt: u32) -> u64 {
    match attempt {
        0 => 0,
        n => ERROR_BACKOFF_STEPS_MS
            .get(n as usize - 1)
            .copied()
            .unwrap_or(ERROR_BACKOFF_CAP_MS),
    }
}

impl Poller {
    pub fn new(name: &'static str, ladder: Ladder, background: Option<Ladder>) -> Arc<Self> {
        Arc::new(Self {
            name,
            control: Mutex::new(Control {
                foreground: ladder,
                background,
                backgrounded: false,
                triggered_at: Instant::now(),
                consecutive_errors: 0,
            }),
            wake: Notify::new(),
            task: Mutex::new(None),
        })
    }

    /// Begins polling with an immediate first run. No-op when running.
    pub fn start<J: PollJob>(self: &Arc<Self>, job: Arc<J>) {
        let mut task = self.task.lock();
        if task.is_some() {
            return;
        }
        {
            let mut control = self.control.lock();
            control.triggered_at = Instant::now();
            control.consecutive_errors = 0;
        }
        let poller = Arc::clone(self);
        *task = Some(tauri::async_runtime::spawn(async move {
            poller.run_loop(job).await
        }));
    }

    /// Stops polling; an in-flight cycle is dropped (aborted).
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        self.control.lock().consecutive_errors = 0;
    }

    /// Resets to the top of the ladder. A trigger during a cycle schedules
    /// an immediate rerun once it completes (the stored `Notify` permit).
    pub fn trigger(&self) {
        if self.task.lock().is_none() {
            return;
        }
        {
            let mut control = self.control.lock();
            control.triggered_at = Instant::now();
            control.consecutive_errors = 0;
        }
        self.wake.notify_one();
    }

    pub fn set_backgrounded(&self, value: bool) {
        self.control.lock().backgrounded = value;
    }

    /// Replaces the ladders without resetting the trigger timestamp.
    #[allow(dead_code)] // used once the realtime socket is ported
    pub fn set_ladder(&self, ladder: Ladder, background: Option<Ladder>) {
        let mut control = self.control.lock();
        control.foreground = ladder;
        control.background = background;
    }

    fn next_delay(&self) -> Duration {
        let control = self.control.lock();
        let ladder = match control.background {
            Some(bg) if control.backgrounded && !bg.is_empty() => bg,
            _ => control.foreground,
        };
        let elapsed = control.triggered_at.elapsed().as_millis() as u64;
        let tier = pick_tier(elapsed, ladder);
        Duration::from_millis(
            tier.interval_ms
                .max(error_backoff(control.consecutive_errors)),
        )
    }

    async fn run_loop<J: PollJob>(self: Arc<Self>, job: Arc<J>) {
        loop {
            if job.should_run() {
                match job.run().await {
                    Ok(()) => self.control.lock().consecutive_errors = 0,
                    Err(error) => {
                        let attempt = {
                            let mut control = self.control.lock();
                            control.consecutive_errors += 1;
                            control.consecutive_errors
                        };
                        tracing::warn!(poller = self.name, attempt, %error, "[Poller] run failed");
                    }
                }
            }
            let delay = self.next_delay();
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.wake.notified() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LADDER: Ladder = &[
        Tier {
            interval_ms: 30_000,
            duration_ms: 60_000,
        },
        Tier {
            interval_ms: 300_000,
            duration_ms: FOREVER,
        },
    ];

    #[test]
    fn walks_down_the_ladder() {
        assert_eq!(pick_tier(0, LADDER).interval_ms, 30_000);
        assert_eq!(pick_tier(59_999, LADDER).interval_ms, 30_000);
        assert_eq!(pick_tier(60_000, LADDER).interval_ms, 300_000);
        assert_eq!(pick_tier(u64::MAX - 1, LADDER).interval_ms, 300_000);
    }

    #[test]
    fn backoff_matches_electron() {
        let steps: Vec<u64> = (0..8).map(error_backoff).collect();
        assert_eq!(
            steps,
            [0, 1_000, 2_000, 5_000, 15_000, 30_000, 60_000, 60_000]
        );
    }
}
