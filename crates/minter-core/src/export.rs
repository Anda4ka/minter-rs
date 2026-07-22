//! Export mint results to JSON / CSV under `results/`.

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use serde::Serialize;

use crate::types::WalletStatus;

#[derive(Debug, Clone, Serialize)]
pub struct MintWalletExport {
    pub address: String,
    pub status: String,
    pub tx_hash: Option<String>,
    pub gas_used: Option<u64>,
    pub block_number: Option<u64>,
    pub error: Option<String>,
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MintRunExport {
    pub slug: String,
    pub chain: String,
    pub phase: String,
    pub started_at: String,
    pub elapsed_ms: u64,
    pub quiet: bool,
    pub skip_preflight: bool,
    pub dry_run: bool,
    pub wallets: Vec<MintWalletExport>,
    pub confirmed: usize,
    pub failed: usize,
    pub total: usize,
}

pub fn wallet_row(
    address: Address,
    status: WalletStatus,
    tx_hash: Option<B256>,
    gas_used: Option<u64>,
    block_number: Option<u64>,
    error: Option<String>,
    proxy: Option<String>,
) -> MintWalletExport {
    MintWalletExport {
        address: format!("{:?}", address),
        status: status.to_string(),
        tx_hash: tx_hash.map(|h| format!("0x{}", hex::encode(h.as_slice()))),
        gas_used,
        block_number,
        error,
        proxy,
    }
}

fn results_dir() -> PathBuf {
    PathBuf::from("results")
}

fn safe_slug(slug: &str) -> String {
    slug.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write JSON + CSV; returns paths written.
pub fn write_mint_results(run: &MintRunExport) -> Result<(PathBuf, PathBuf)> {
    let dir = results_dir();
    std::fs::create_dir_all(&dir).context("create results/")?;

    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let base = format!("mint_{}_{}", safe_slug(&run.slug), ts);
    let json_path = dir.join(format!("{}.json", base));
    let csv_path = dir.join(format!("{}.csv", base));

    let json = serde_json::to_string_pretty(run).context("serialize mint results")?;
    std::fs::write(&json_path, json)
        .with_context(|| format!("write {}", json_path.display()))?;

    let mut csv = String::from(
        "address,status,tx_hash,gas_used,block_number,error,proxy\n",
    );
    for w in &run.wallets {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            csv_escape(&w.address),
            csv_escape(&w.status),
            csv_escape(w.tx_hash.as_deref().unwrap_or("")),
            w.gas_used.map(|g| g.to_string()).unwrap_or_default(),
            w.block_number.map(|b| b.to_string()).unwrap_or_default(),
            csv_escape(w.error.as_deref().unwrap_or("")),
            csv_escape(w.proxy.as_deref().unwrap_or("")),
        ));
    }
    std::fs::write(&csv_path, csv).with_context(|| format!("write {}", csv_path.display()))?;

    Ok((json_path, csv_path))
}

/// Write a run's structured metrics (plan #9) as JSON next to the results.
pub fn write_run_metrics(m: &crate::metrics::RunMetrics) -> Result<PathBuf> {
    let dir = results_dir();
    std::fs::create_dir_all(&dir).context("create results/")?;
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("metrics_{}_{}.json", safe_slug(&m.slug), ts));
    let json = serde_json::to_string_pretty(m).context("serialize run metrics")?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn path_display(p: &Path) -> String {
    p.display().to_string()
}

// ── WL eligibility export (os.py-style: per-phase files + CSV) ───────────────

/// One CSV row: wallet eligible for a **non-public** WL stage.
#[derive(Debug, Clone)]
pub struct WlCsvRow {
    pub address: String,
    pub stage: String,
    pub max_mint: String,
}

/// Paths written by [`write_wl_eligibility_export`].
#[derive(Debug, Clone)]
pub struct WlExportPaths {
    pub dir: PathBuf,
    pub csv: PathBuf,
    pub not_eligible: PathBuf,
    /// Stage key → path (e.g. `SIGNED_PRESALE#0.txt`)
    pub stage_files: Vec<(String, PathBuf)>,
}

/// Sanitize OpenSea stage type+index for a Windows-safe filename stem.
pub fn wl_stage_file_key(stage_type: &str, stage_index: Option<i64>) -> String {
    let raw = match stage_index {
        Some(i) => format!("{stage_type}#{i}"),
        None => stage_type.to_string(),
    };
    raw.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// Write WL check results:
/// - `eligibility.csv` — address,stage,max_mint (WL eligible only, **no PUBLIC_SALE**)
/// - one `{STAGE}.txt` per WL phase (addresses)
/// - `not_eligible.txt` — wallets with no WL-eligible phase (public-only / none / errors)
///
/// `stage_wallets`: stage_key → addresses (e.g. SIGNED_PRESALE#0).
pub fn write_wl_eligibility_export(
    slug: &str,
    csv_rows: &[WlCsvRow],
    stage_wallets: &[(String, Vec<String>)],
    not_eligible: &[String],
) -> Result<WlExportPaths> {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let dir = results_dir().join(format!("wl_{}_{}", safe_slug(slug), ts));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let csv_path = dir.join("eligibility.csv");
    let mut csv = String::from("address,stage,max_mint\n");
    for r in csv_rows {
        csv.push_str(&format!(
            "{},{},{}\n",
            csv_escape(&r.address),
            csv_escape(&r.stage),
            csv_escape(&r.max_mint),
        ));
    }
    std::fs::write(&csv_path, csv).with_context(|| format!("write {}", csv_path.display()))?;

    let mut stage_files = Vec::new();
    for (key, addrs) in stage_wallets {
        if addrs.is_empty() {
            continue;
        }
        let path = dir.join(format!("{key}.txt"));
        let mut body = String::new();
        for a in addrs {
            body.push_str(a.trim());
            body.push('\n');
        }
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        stage_files.push((key.clone(), path));
    }

    let not_path = dir.join("not_eligible.txt");
    let mut not_body = String::new();
    for a in not_eligible {
        not_body.push_str(a.trim());
        not_body.push('\n');
    }
    std::fs::write(&not_path, not_body)
        .with_context(|| format!("write {}", not_path.display()))?;

    Ok(WlExportPaths {
        dir,
        csv: csv_path,
        not_eligible: not_path,
        stage_files,
    })
}

#[cfg(test)]
mod wl_export_tests {
    use super::*;

    #[test]
    fn stage_file_key_safe() {
        assert_eq!(
            wl_stage_file_key("SIGNED_PRESALE", Some(0)),
            "SIGNED_PRESALE#0"
        );
        assert!(!wl_stage_file_key("A:B", Some(1)).contains(':'));
    }

    #[test]
    fn write_wl_export_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "minter_wl_export_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // redirect by writing into a unique slug that lands under results/ — use write + cwd
        // Instead call write with temp: monkey by using absolute via chdir is bad.
        // Unit-test pure key + CSV content via write into results under unique slug then cleanup.
        let slug = format!("testexport{}", std::process::id());
        let rows = vec![WlCsvRow {
            address: "0xabc".into(),
            stage: "SIGNED_PRESALE#0".into(),
            max_mint: "2".into(),
        }];
        let stages = vec![("SIGNED_PRESALE#0".into(), vec!["0xabc".into()])];
        let not_elig = vec!["0xdef".into()];
        let out = write_wl_eligibility_export(&slug, &rows, &stages, &not_elig).unwrap();
        assert!(out.csv.exists());
        let csv = std::fs::read_to_string(&out.csv).unwrap();
        assert!(csv.contains("address,stage,max_mint"));
        assert!(csv.contains("0xabc"));
        assert!(csv.contains("SIGNED_PRESALE#0"));
        assert!(!csv.to_lowercase().contains("public_sale"));
        let stage_txt = std::fs::read_to_string(&out.stage_files[0].1).unwrap();
        assert!(stage_txt.contains("0xabc"));
        let ne = std::fs::read_to_string(&out.not_eligible).unwrap();
        assert!(ne.contains("0xdef"));
        let _ = std::fs::remove_dir_all(&out.dir);
        let _ = dir; // silence
    }
}
