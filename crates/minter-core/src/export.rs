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
