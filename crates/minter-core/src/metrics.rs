//! Structured per-run latency / outcome metrics (plan item #9).
//!
//! The goal is to answer "where did the milliseconds go?" and "why did wallets
//! fail?" for a mint run, using only local JSON + the History UI — no cloud.
//!
//! This module is the **data model + collector**. It is deliberately decoupled
//! from the mint hot path: callers build a [`MetricsCollector`], mark spans /
//! wallet outcomes as they go, and call [`MetricsCollector::finish`] to get a
//! serializable [`RunMetrics`]. Wiring the collector into the OpenSea / raw
//! paths is a separate, follow-up step so the hot path is touched carefully.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Wall-clock milliseconds since the Unix epoch (best effort; 0 if the clock is
/// before the epoch, which never happens in practice).
pub fn unix_ms_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Coarse failure class for a wallet error, derived from the shared
/// [`crate::errors`] taxonomy plus a couple of transport/cancel hints.
/// One of: `funds`, `fatal`, `rpc`, `cancel`, `retryable`.
pub fn error_class(msg: &str) -> &'static str {
    let lower = msg.to_lowercase();
    if lower.contains("cancel") || lower.contains("aborted") {
        return "cancel";
    }
    use crate::errors::TxErrorKind::*;
    match crate::errors::classify(msg) {
        InsufficientFunds => "funds",
        FatalContract => "fatal",
        NonceTooLow | Underpriced | IntrinsicGasTooLow | AlreadyKnown => "retryable",
        Retryable => {
            // Distinguish transport/RPC noise from generic contract retryables.
            if lower.contains("timeout")
                || lower.contains("rpc")
                || lower.contains("http")
                || lower.contains("connection")
                || lower.contains("dns")
            {
                "rpc"
            } else {
                "retryable"
            }
        }
    }
}

/// Per-phase span durations for a run (milliseconds). All optional so a path
/// only fills the spans it actually measures.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunSpans {
    pub auth_ms: Option<u64>,
    pub nonce_ms: Option<u64>,
    pub balance_ms: Option<u64>,
    pub wait_ms: Option<u64>,
    pub prefetch_ms: Option<u64>,
    pub prep_sign_ms: Option<u64>,
    /// Offset from t0 to the first accepted broadcast.
    pub first_send_ack_ms: Option<u64>,
    pub first_confirm_ms: Option<u64>,
    pub done_ms: Option<u64>,
}

/// How/when the mint "opened" and how close to t0 we fired.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OpenSignalMetrics {
    pub kind: String,
    pub open_ts: Option<i64>,
    pub fire_lag_ms: i64,
}

/// Per-wallet outcome + timings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WalletMetrics {
    pub address: String,
    pub proxy: Option<String>,
    pub status: String,
    pub send_attempts: u32,
    pub t_send_ack_ms: Option<u64>,
    pub t_confirm_ms: Option<u64>,
    pub tx_hash: Option<String>,
    pub gas_used: Option<u64>,
    pub error: Option<String>,
    /// Derived from `error` via [`error_class`]; `None` when there is no error.
    pub error_class: Option<String>,
}

impl WalletMetrics {
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            proxy: None,
            status: String::new(),
            send_attempts: 0,
            t_send_ack_ms: None,
            t_confirm_ms: None,
            tx_hash: None,
            gas_used: None,
            error: None,
            error_class: None,
        }
    }

    /// Record an error string and classify it.
    pub fn with_error(mut self, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        self.error_class = Some(error_class(&msg).to_string());
        self.error = Some(msg);
        self
    }

    fn is_confirmed(&self) -> bool {
        self.error.is_none()
            && (self.tx_hash.is_some() || {
                let s = self.status.to_lowercase();
                s.contains("confirm") || s == "ok"
            })
    }
}

/// Aggregate summary computed at [`MetricsCollector::finish`].
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunMetricsSummary {
    pub total: usize,
    pub confirmed: usize,
    pub failed: usize,
    pub elapsed_ms: u64,
    /// Histogram of failure classes (funds/fatal/rpc/cancel/retryable).
    pub fail_reasons: HashMap<String, usize>,
}

/// Full metrics record for one run.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunMetrics {
    pub run_id: String,
    pub kind: String,
    pub slug: String,
    pub chain: String,
    pub dry_run: bool,
    pub open_mode: Option<String>,
    pub t0_unix_ms: i64,
    pub spans: RunSpans,
    pub open_signal: Option<OpenSignalMetrics>,
    pub wallets: Vec<WalletMetrics>,
    pub summary: RunMetricsSummary,
}

/// Thread-safe collector shared (via `Arc`) across the wallet tasks of a run.
///
/// Critical sections are short and never `.await` while holding the lock, so a
/// plain `std::sync::Mutex` is appropriate.
pub struct MetricsCollector {
    inner: Mutex<RunMetrics>,
    t0: Instant,
}

impl MetricsCollector {
    pub fn new(
        kind: impl Into<String>,
        slug: impl Into<String>,
        chain: impl Into<String>,
        dry_run: bool,
        open_mode: Option<String>,
    ) -> Self {
        let slug = slug.into();
        let t0_unix_ms = unix_ms_now();
        let run_id = format!(
            "{}_{}",
            if slug.is_empty() { "run" } else { &slug },
            t0_unix_ms
        );
        let metrics = RunMetrics {
            run_id,
            kind: kind.into(),
            slug,
            chain: chain.into(),
            dry_run,
            open_mode,
            t0_unix_ms,
            ..Default::default()
        };
        Self {
            inner: Mutex::new(metrics),
            t0: Instant::now(),
        }
    }

    /// Milliseconds since the collector was created (run t0).
    pub fn elapsed_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    /// Set a named span duration. Unknown names are ignored (forward-compatible).
    pub fn mark_span(&self, name: &str, ms: u64) {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let s = &mut m.spans;
        match name {
            "auth" => s.auth_ms = Some(ms),
            "nonce" => s.nonce_ms = Some(ms),
            "balance" => s.balance_ms = Some(ms),
            "wait" => s.wait_ms = Some(ms),
            "prefetch" => s.prefetch_ms = Some(ms),
            "prep_sign" => s.prep_sign_ms = Some(ms),
            "first_send_ack" => s.first_send_ack_ms = Some(ms),
            "first_confirm" => s.first_confirm_ms = Some(ms),
            "done" => s.done_ms = Some(ms),
            _ => {}
        }
    }

    pub fn set_open_signal(&self, kind: impl Into<String>, open_ts: Option<i64>, fire_lag_ms: i64) {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        m.open_signal = Some(OpenSignalMetrics {
            kind: kind.into(),
            open_ts,
            fire_lag_ms,
        });
    }

    /// Insert or replace the metrics for a wallet (matched by address).
    pub fn upsert_wallet(&self, w: WalletMetrics) {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = m.wallets.iter_mut().find(|x| x.address == w.address) {
            *existing = w;
        } else {
            m.wallets.push(w);
        }
    }

    /// Snapshot the current metrics with a freshly computed summary.
    pub fn finish(&self) -> RunMetrics {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone();
        m.summary = Self::summarize(&m.wallets, self.elapsed_ms());
        if m.spans.done_ms.is_none() {
            m.spans.done_ms = Some(self.elapsed_ms());
        }
        m
    }

    fn summarize(wallets: &[WalletMetrics], elapsed_ms: u64) -> RunMetricsSummary {
        let mut summary = RunMetricsSummary {
            total: wallets.len(),
            elapsed_ms,
            ..Default::default()
        };
        for w in wallets {
            if w.error.is_some() {
                summary.failed += 1;
                let class = w
                    .error_class
                    .clone()
                    .unwrap_or_else(|| "retryable".to_string());
                *summary.fail_reasons.entry(class).or_insert(0) += 1;
            } else if w.is_confirmed() {
                summary.confirmed += 1;
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_class_mapping() {
        assert_eq!(error_class("insufficient funds for gas"), "funds");
        assert_eq!(error_class("execution reverted: InvalidProof"), "fatal");
        assert_eq!(error_class("nonce too low"), "retryable");
        assert_eq!(
            error_class("replacement transaction underpriced"),
            "retryable"
        );
        assert_eq!(error_class("request timeout"), "rpc");
        assert_eq!(error_class("RPC connection reset"), "rpc");
        assert_eq!(error_class("user canceled mint"), "cancel");
        assert_eq!(error_class("execution reverted"), "retryable");
    }

    #[test]
    fn summary_counts_and_histogram() {
        let c = MetricsCollector::new("opensea", "cool-drop", "base", false, Some("public".into()));
        c.upsert_wallet(WalletMetrics {
            tx_hash: Some("0xabc".into()),
            status: "confirmed".into(),
            ..WalletMetrics::new("0xA")
        });
        c.upsert_wallet(WalletMetrics::new("0xB").with_error("insufficient funds"));
        c.upsert_wallet(WalletMetrics::new("0xC").with_error("request timeout"));
        c.upsert_wallet(WalletMetrics::new("0xD").with_error("insufficient balance"));
        let m = c.finish();
        assert_eq!(m.summary.total, 4);
        assert_eq!(m.summary.confirmed, 1);
        assert_eq!(m.summary.failed, 3);
        assert_eq!(m.summary.fail_reasons.get("funds"), Some(&2));
        assert_eq!(m.summary.fail_reasons.get("rpc"), Some(&1));
        assert!(m.run_id.starts_with("cool-drop_"));
        assert_eq!(m.kind, "opensea");
    }

    #[test]
    fn upsert_replaces_by_address() {
        let c = MetricsCollector::new("rawSniper", "", "ethereum", true, None);
        c.upsert_wallet(WalletMetrics::new("0xA")); // pending
        c.upsert_wallet(WalletMetrics {
            tx_hash: Some("0xdead".into()),
            status: "confirmed".into(),
            ..WalletMetrics::new("0xA")
        });
        let m = c.finish();
        assert_eq!(m.wallets.len(), 1, "same address updates, not duplicates");
        assert_eq!(m.summary.confirmed, 1);
        assert!(m.run_id.starts_with("run_"), "empty slug → run_<ts>");
    }

    #[test]
    fn spans_and_open_signal_recorded() {
        let c = MetricsCollector::new("rawSniper", "s", "ethereum", false, None);
        c.mark_span("prep_sign", 42);
        c.mark_span("first_send_ack", 120);
        c.mark_span("unknown_span_ignored", 999);
        c.set_open_signal("mintbay_view", Some(1_700_000_000), -8);
        let m = c.finish();
        assert_eq!(m.spans.prep_sign_ms, Some(42));
        assert_eq!(m.spans.first_send_ack_ms, Some(120));
        assert!(m.spans.done_ms.is_some());
        let sig = m.open_signal.unwrap();
        assert_eq!(sig.kind, "mintbay_view");
        assert_eq!(sig.fire_lag_ms, -8);
    }

    #[test]
    fn serde_roundtrip_camel_case() {
        let c = MetricsCollector::new("opensea", "d", "base", false, None);
        c.mark_span("auth", 10);
        c.upsert_wallet(WalletMetrics {
            t_send_ack_ms: Some(5),
            ..WalletMetrics::new("0xA")
        });
        let m = c.finish();
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"authMs\":10"));
        assert!(json.contains("\"tSendAckMs\":5"));
        assert!(json.contains("\"t0UnixMs\""));
        let back: RunMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
