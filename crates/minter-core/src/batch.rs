//! Streaming progress for batch wallet operations (WL check, auth test,
//! balances, latency probes…).
//!
//! These operations run N wallets behind a semaphore and used to return only a
//! finished report, so a 200-wallet run showed nothing for over a minute. The
//! reporter here lets each worker publish its row the moment it is done, and
//! lets the operator stop a run in flight.

use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Default worker count when the caller doesn't specify one.
pub const DEFAULT_BATCH_CONCURRENCY: usize = 4;
/// Upper bound offered to the operator. Above this, OpenSea rate-limits hard.
pub const MAX_BATCH_CONCURRENCY: usize = 16;

/// Clamp a requested worker count into the supported range.
///
/// Without proxies every worker shares one IP, so more than a single in-flight
/// SIWE is a fast route to a 429 storm — the request is capped at 1 in that
/// case regardless of what the UI asked for.
pub fn resolve_concurrency(requested: Option<usize>, proxy_count: usize) -> usize {
    let want = requested.unwrap_or(DEFAULT_BATCH_CONCURRENCY);
    if proxy_count == 0 {
        return 1;
    }
    want.clamp(1, MAX_BATCH_CONCURRENCY)
}

/// One streamed update from a batch operation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEvent {
    /// Which batch this belongs to (`wlCheck`, `authTest`, `balances`, …), so a
    /// single event channel can serve every screen.
    pub kind: String,
    /// Wallets finished so far (including failures).
    pub done: usize,
    /// Total wallets in this run.
    pub total: usize,
    /// The finished row, serialized by the caller. `None` for progress-only
    /// events such as the initial "started" tick.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<serde_json::Value>,
    /// Set when the run stopped early because the operator cancelled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    /// Free-form status line for the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl BatchEvent {
    /// A finished wallet row.
    pub fn row(kind: &str, done: usize, total: usize, row: serde_json::Value) -> Self {
        Self {
            kind: kind.to_string(),
            done,
            total,
            row: Some(row),
            cancelled: None,
            message: None,
        }
    }

    /// A progress / status tick with no row attached.
    pub fn message(kind: &str, done: usize, total: usize, message: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            done,
            total,
            row: None,
            cancelled: None,
            message: Some(message.into()),
        }
    }

    /// Terminal event for a run the operator stopped.
    pub fn cancelled(kind: &str, done: usize, total: usize) -> Self {
        Self {
            kind: kind.to_string(),
            done,
            total,
            row: None,
            cancelled: Some(true),
            message: Some(format!("stopped by operator after {done}/{total}")),
        }
    }
}

/// Sink for batch progress. The desktop implements this by emitting a Tauri
/// event; tests and headless callers can drop the events.
pub trait BatchReporter: Send + Sync {
    fn report(&self, event: BatchEvent);
}

/// Discards every event (headless / library use).
pub struct NullBatchReporter;

impl BatchReporter for NullBatchReporter {
    fn report(&self, _event: BatchEvent) {}
}

/// Collects events in memory (tests).
#[derive(Default)]
pub struct CollectingBatchReporter {
    pub events: std::sync::Mutex<Vec<BatchEvent>>,
}

impl BatchReporter for CollectingBatchReporter {
    fn report(&self, event: BatchEvent) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
}

impl CollectingBatchReporter {
    pub fn take(&self) -> Vec<BatchEvent> {
        self.events
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

/// Cooperative cancel flag shared with a running batch.
#[derive(Clone, Default)]
pub struct BatchCancel(Arc<AtomicBool>);

impl BatchCancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Wrap an existing flag (the desktop keeps one in app state).
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Emit an event if a reporter is present.
pub fn report(reporter: Option<&Arc<dyn BatchReporter>>, event: BatchEvent) {
    if let Some(r) = reporter {
        r.report(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_is_forced_serial_without_proxies() {
        // One IP: more than one in-flight SIWE invites a 429 storm.
        assert_eq!(resolve_concurrency(Some(8), 0), 1);
        assert_eq!(resolve_concurrency(None, 0), 1);
    }

    #[test]
    fn concurrency_honors_request_with_proxies() {
        assert_eq!(resolve_concurrency(Some(4), 10), 4);
        assert_eq!(resolve_concurrency(Some(1), 10), 1);
        assert_eq!(resolve_concurrency(None, 10), DEFAULT_BATCH_CONCURRENCY);
    }

    #[test]
    fn concurrency_is_bounded() {
        assert_eq!(resolve_concurrency(Some(999), 10), MAX_BATCH_CONCURRENCY);
        assert_eq!(resolve_concurrency(Some(0), 10), 1);
    }

    #[test]
    fn cancel_flag_roundtrip() {
        let c = BatchCancel::new();
        assert!(!c.is_cancelled());
        c.cancel();
        assert!(c.is_cancelled());
        // Clones share the same flag.
        let c2 = c.clone();
        assert!(c2.is_cancelled());
        c2.reset();
        assert!(!c.is_cancelled());
    }

    #[test]
    fn collecting_reporter_records_events() {
        let r = CollectingBatchReporter::default();
        r.report(BatchEvent::row(
            "wlCheck",
            1,
            3,
            serde_json::json!({"a": 1}),
        ));
        r.report(BatchEvent::cancelled("wlCheck", 1, 3));
        let evs = r.take();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].done, 1);
        assert_eq!(evs[0].total, 3);
        assert!(evs[0].row.is_some());
        assert_eq!(evs[1].cancelled, Some(true));
        assert!(r.take().is_empty(), "take drains");
    }

    /// Proves the JoinSet drain pattern used by `check_eligibility_wallets_streaming`:
    /// rows surface as they finish (not in spawn order), and cancel stops the queue
    /// while keeping what already completed.
    #[tokio::test]
    async fn joinset_streams_in_completion_order_and_cancels() {
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        let reporter = Arc::new(CollectingBatchReporter::default());
        let cancel = BatchCancel::new();
        let conc = resolve_concurrency(Some(4), 10);
        assert_eq!(conc, 4);

        let sem = Arc::new(Semaphore::new(conc));
        let mut set: tokio::task::JoinSet<usize> = tokio::task::JoinSet::new();
        // Later tasks finish sooner, so completion order != spawn order.
        for i in 0..8usize {
            let sem = sem.clone();
            let cancel_w = cancel.clone();
            set.spawn(async move {
                let _p = sem.acquire().await.unwrap();
                if cancel_w.is_cancelled() {
                    return usize::MAX;
                }
                tokio::time::sleep(std::time::Duration::from_millis(((8 - i) * 5) as u64)).await;
                i
            });
        }

        let total = 8;
        let mut done = 0usize;
        let mut order = Vec::new();
        while let Some(joined) = set.join_next().await {
            // Mirror production: aborted tasks drain as JoinError::Cancelled and
            // must be skipped, not recorded.
            let v = match joined {
                Ok(v) => v,
                Err(e) if e.is_cancelled() => continue,
                Err(e) => panic!("unexpected join error: {e}"),
            };
            done += 1;
            order.push(v);
            reporter.report(BatchEvent::row("t", done, total, serde_json::json!(v)));
            if done == 3 {
                cancel.cancel();
                set.abort_all();
                reporter.report(BatchEvent::cancelled("t", done, total));
            }
        }

        let evs = reporter.take();
        // Rows were emitted incrementally, not all at the end.
        let rows: Vec<_> = evs.iter().filter(|e| e.row.is_some()).collect();
        assert!(
            rows.len() >= 3,
            "expected streamed rows, got {}",
            rows.len()
        );
        assert_eq!(rows[0].done, 1, "first row reported at done=1");
        assert_eq!(rows[0].total, total);
        // Cancellation was reported and stopped the run short of the full set.
        assert!(evs.iter().any(|e| e.cancelled == Some(true)));
        assert!(done < total, "cancel must stop the queue: {done}/{total}");
        // Completion order differs from spawn order (later tasks slept less).
        assert_ne!(order, (0..done).collect::<Vec<_>>());
    }
}
