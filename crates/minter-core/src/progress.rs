//! Mint progress reporting (CLI dashboard / Tauri events).

use alloy_primitives::{Address, B256};
use serde::Serialize;
use std::sync::Mutex;

use crate::types::WalletStatus;

/// Event emitted during mint for UI layers (JSON-friendly for Tauri).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintEvent {
    pub address: Option<String>,
    pub status: Option<String>,
    pub detail: Option<String>,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub message: Option<String>,
    /// High-level phase for UI banner: prep | auth | wait | fire | confirm | done | error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Human-readable one-liner for the phase banner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_label: Option<String>,
}

/// Redact secrets from free-form mint log strings (JWT, long hex keys).
pub fn sanitize_log_text(s: &str) -> String {
    let mut out = s.to_string();
    // Bearer / JWT-like.
    // ASCII-lowercase only: `to_lowercase()` applies full Unicode case mapping,
    // which can change byte lengths (e.g. `İ` 2 bytes → `i̇` 3 bytes), so its
    // byte index is invalid for slicing `out` — that either panics mid-char or
    // shifts the slice so redaction silently fails and the token reaches the log.
    if let Some(idx) = out.to_ascii_lowercase().find("bearer ") {
        let rest = &out[idx + 7..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if end > 12 {
            out = format!("{}Bearer ***REDACTED***{}", &out[..idx], &rest[end..]);
        }
    }
    // eyJ… JWT payload
    let mut cleaned = String::with_capacity(out.len());
    for part in out.split_whitespace() {
        if part.starts_with("eyJ") && part.len() > 20 {
            cleaned.push_str("***JWT***");
        } else if part.len() >= 64
            && part
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == 'x' || c == 'X')
        {
            // long hex blobs (keys) — keep short prefixes only if 0x address
            if part.starts_with("0x") && part.len() == 42 {
                cleaned.push_str(part);
            } else {
                cleaned.push_str("***HEX***");
            }
        } else {
            cleaned.push_str(part);
        }
        cleaned.push(' ');
    }
    cleaned.trim_end().to_string()
}

impl MintEvent {
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            address: None,
            status: None,
            detail: None,
            tx_hash: None,
            error: None,
            message: Some(sanitize_log_text(&msg.into())),
            phase: None,
            phase_label: None,
        }
    }

    /// UI banner phase update (human-readable).
    pub fn phase(phase: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            address: None,
            status: None,
            detail: None,
            tx_hash: None,
            error: None,
            message: None,
            phase: Some(phase.into()),
            phase_label: Some(sanitize_log_text(&label.into())),
        }
    }

    pub fn wallet(
        address: Address,
        status: Option<WalletStatus>,
        detail: Option<String>,
        tx_hash: Option<B256>,
        error: Option<String>,
    ) -> Self {
        Self {
            address: Some(format!("{:?}", address)),
            status: status.map(|s| s.to_string()),
            detail: detail.map(|d| sanitize_log_text(&d)),
            tx_hash: tx_hash.map(|h| format!("0x{}", hex::encode(h.as_slice()))),
            error: error.map(|e| sanitize_log_text(&e)),
            message: None,
            phase: None,
            phase_label: None,
        }
    }
}

/// Sink for mint progress (CLI dashboard / desktop emit).
pub trait MintReporter: Send + Sync {
    fn report(&self, event: MintEvent);
}

/// No-op reporter for tests / headless runs.
pub struct NullReporter;

impl MintReporter for NullReporter {
    fn report(&self, _event: MintEvent) {}
}

/// Tee events to an inner reporter and a log file (full verbose trail).
pub struct FileTeeReporter {
    pub inner: std::sync::Arc<dyn MintReporter>,
    pub file: Mutex<std::fs::File>,
    pub path: std::path::PathBuf,
}

impl FileTeeReporter {
    pub fn create(inner: std::sync::Arc<dyn MintReporter>, slug: &str) -> std::io::Result<Self> {
        let dir = std::path::PathBuf::from("logs");
        std::fs::create_dir_all(&dir)?;
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let safe: String = slug
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(40)
            .collect();
        let path = dir.join(format!("mint_{safe}_{ts}.log"));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        use std::io::Write;
        let _ = writeln!(
            file,
            "# mint log slug={slug} started={}",
            chrono::Utc::now().to_rfc3339()
        );
        Ok(Self {
            inner,
            file: Mutex::new(file),
            path,
        })
    }
}

impl MintReporter for FileTeeReporter {
    fn report(&self, event: MintEvent) {
        if let Ok(mut f) = self.file.lock() {
            use std::io::Write;
            let ts = chrono::Utc::now().format("%H:%M:%S%.3f");
            if let Some(ref m) = event.message {
                let _ = writeln!(f, "[{ts}] {m}");
            }
            if let Some(ref phase) = event.phase {
                let lab = event.phase_label.as_deref().unwrap_or("");
                let _ = writeln!(f, "[{ts}] PHASE {phase}: {lab}");
            }
            if let Some(ref addr) = event.address {
                let _ = writeln!(
                    f,
                    "[{ts}] wallet={} status={:?} detail={:?} tx={:?} err={:?}",
                    addr, event.status, event.detail, event.tx_hash, event.error
                );
            }
            let _ = f.flush();
        }
        self.inner.report(event);
    }
}

/// Collects events in memory (tests / simple UIs).
pub struct CollectingReporter {
    pub events: Mutex<Vec<MintEvent>>,
}

impl CollectingReporter {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn drain(&self) -> Vec<MintEvent> {
        self.events
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

impl Default for CollectingReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl MintReporter for CollectingReporter {
    fn report(&self, event: MintEvent) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
}

/// Prints messages to stdout (CLI bridge when TUI dashboard is not used).
pub struct StdoutReporter {
    pub quiet: bool,
}

impl MintReporter for StdoutReporter {
    fn report(&self, event: MintEvent) {
        if let Some(msg) = event.message {
            if !self.quiet {
                println!("{}", msg);
            }
            return;
        }
        if self.quiet {
            return;
        }
        let addr = event.address.as_deref().unwrap_or("-");
        let status = event.status.as_deref().unwrap_or("-");
        let detail = event.detail.as_deref().unwrap_or("");
        let err = event.error.as_deref().unwrap_or("");
        let tx = event.tx_hash.as_deref().unwrap_or("");
        println!("[{}] {} {} {} {}", addr, status, detail, tx, err);
    }
}

#[cfg(test)]
mod sanitize_unicode_tests {
    use super::sanitize_log_text;

    #[test]
    fn redacts_bearer_after_multibyte_chars() {
        // `İ` (U+0130) grows from 2 to 3 bytes under full Unicode lowercasing,
        // which used to shift the byte index and panic or skip redaction.
        let s = "İ Bearer abcdefghijklmnopqrstuvwxyz rest";
        let out = sanitize_log_text(s);
        assert!(out.contains("REDACTED"), "got: {out}");
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz"), "got: {out}");

        // `ẞ` (U+1E9E) shrinks 3 -> 2 bytes when lowercased.
        let s2 = "ẞẞẞ Bearer abcdefghijklmnopqrstuvwxyz tail";
        let out2 = sanitize_log_text(s2);
        assert!(!out2.contains("abcdefghijklmnopqrstuvwxyz"), "got: {out2}");
    }

    #[test]
    fn plain_ascii_bearer_still_redacted() {
        let out = sanitize_log_text("auth Bearer abcdefghijklmnopqrstuv done");
        assert!(out.contains("REDACTED"));
        assert!(out.contains("done"));
    }
}
