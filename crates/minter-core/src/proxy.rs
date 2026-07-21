use anyhow::{Context, Result};

#[derive(Clone, Default)]
pub struct ProxyManager {
    proxies: Vec<String>,
}

/// Short label for logs/UI (hides credentials when possible).
pub fn short_proxy(url: &str) -> String {
    if let Some(at) = url.find('@') {
        let _scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
        let host_part = &url[at + 1..];
        if host_part.len() > 30 {
            format!("{}...{}", &host_part[..14], &host_part[host_part.len() - 8..])
        } else {
            host_part.to_string()
        }
    } else {
        let stripped = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .or_else(|| url.strip_prefix("socks5://"))
            .or_else(|| url.strip_prefix("socks4://"))
            .unwrap_or(url);
        if stripped.len() > 30 {
            format!("{}...{}", &stripped[..14], &stripped[stripped.len() - 8..])
        } else {
            stripped.to_string()
        }
    }
}

pub fn parse_proxy(raw: &str) -> Result<String> {
    let raw = raw.trim();

    if raw.is_empty() {
        anyhow::bail!("empty proxy string");
    }

    if raw.starts_with("http://")
        || raw.starts_with("https://")
        || raw.starts_with("socks5://")
        || raw.starts_with("socks4://")
        || raw.starts_with("socks5h://")
    {
        return Ok(raw.to_string());
    }

    if raw.contains('@') && !raw.contains("://") {
        return Ok(format!("http://{}", raw));
    }

    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() == 4 {
        let host = parts[0];
        let port = parts[1];
        let user = parts[2];
        let pass = parts[3];
        return Ok(format!("http://{}:{}@{}:{}", user, pass, host, port));
    }

    if parts.len() == 2 {
        return Ok(format!("http://{}", raw));
    }

    Ok(format!("http://{}", raw))
}

impl ProxyManager {
    pub fn new() -> Self {
        Self {
            proxies: Vec::new(),
        }
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read proxy file: {}", path.display()))?;
        Ok(Self::from_text(&content))
    }

    /// Parse multi-line proxy list (same formats as proxies.txt).
    pub fn from_text(content: &str) -> Self {
        let proxies: Vec<String> = content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| parse_proxy(l).ok())
            .collect();
        Self { proxies }
    }

    pub fn from_lines(lines: &[String]) -> Self {
        Self::from_text(&lines.join("\n"))
    }

    pub fn load_default() -> Self {
        let candidates = {
            let mut v = Vec::new();
            if let Ok(cwd) = std::env::current_dir() {
                v.push(cwd.join("proxies.txt"));
            }
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    let mut d = dir;
                    for _ in 0..10 {
                        v.push(d.join("proxies.txt"));
                        match d.parent() {
                            Some(p) => d = p,
                            None => break,
                        }
                    }
                }
            }
            v
        };
        for path in &candidates {
            if path.exists() {
                match Self::from_file(path) {
                    Ok(pm) if !pm.is_empty() => {
                        crate::rlog!("Loaded {} proxy(ies) from {}", pm.len(), path.display());
                        return pm;
                    }
                    Ok(_) => {
                        crate::rlog!("Proxy file {} is empty", path.display());
                    }
                    Err(e) => {
                        crate::rlog!("Failed to load proxies from {}: {}", path.display(), e);
                    }
                }
            }
        }
        Self::new()
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        if self.proxies.is_empty() {
            return None;
        }
        Some(self.proxies[index % self.proxies.len()].as_str())
    }

    pub fn short(&self, index: usize) -> String {
        match self.get(index) {
            Some(p) => short_proxy(p),
            None => "direct".to_string(),
        }
    }

    pub fn len(&self) -> usize {
        self.proxies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.proxies.is_empty()
    }

    /// Remap so new index `j` uses the proxy that vault wallet `indices[j]` had.
    /// Preserves "wallet #i → proxy #i" when only a subset of wallets is selected.
    pub fn remap_for_indices(&self, indices: &[usize]) -> Self {
        self.remap_for_indices_with_overrides(indices, &std::collections::HashMap::new())
    }

    /// Like [`remap_for_indices`], but `overrides` maps vault index → proxy list index.
    pub fn remap_for_indices_with_overrides(
        &self,
        indices: &[usize],
        overrides: &std::collections::HashMap<usize, usize>,
    ) -> Self {
        if self.proxies.is_empty() || indices.is_empty() {
            return Self::new();
        }
        let proxies: Vec<String> = indices
            .iter()
            .filter_map(|&vault_i| {
                let pidx = overrides.get(&vault_i).copied().unwrap_or(vault_i);
                self.get(pidx).map(|s| s.to_string())
            })
            .collect();
        Self { proxies }
    }

    /// All proxy short labels for UI dropdown (index + short).
    pub fn list_short(&self) -> Vec<(usize, String)> {
        self.proxies
            .iter()
            .enumerate()
            .map(|(i, p)| (i, short_proxy(p)))
            .collect()
    }

    /// Probe unique proxies (or a synthetic "direct" entry if empty).
    /// `wallet_count` maps each wallet index → health of its assigned proxy.
    pub async fn probe_for_wallets(&self, wallet_count: usize) -> Vec<ProxyHealth> {
        if wallet_count == 0 {
            return vec![];
        }
        if self.proxies.is_empty() {
            let h = probe_direct().await;
            return (0..wallet_count)
                .map(|i| ProxyHealth {
                    wallet_index: i,
                    label: "direct".into(),
                    ok: h.ok,
                    latency_ms: h.latency_ms,
                    error: h.error.clone(),
                })
                .collect();
        }

        // Probe each unique proxy URL once, then map to wallets.
        let mut unique: Vec<(usize, String)> = Vec::new();
        for (i, p) in self.proxies.iter().enumerate() {
            if !unique.iter().any(|(_, u)| u == p) {
                unique.push((i, p.clone()));
            }
        }
        let mut handles = Vec::new();
        for (idx, url) in unique {
            handles.push(tokio::spawn(async move {
                let health = probe_proxy_url(&url).await;
                (idx, url, health)
            }));
        }
        let mut by_url: std::collections::HashMap<String, ProbeResult> =
            std::collections::HashMap::new();
        for h in handles {
            if let Ok((idx, url, res)) = h.await {
                let _ = idx;
                by_url.insert(url, res);
            }
        }

        (0..wallet_count)
            .map(|i| {
                let url = self.get(i).unwrap_or("");
                let label = self.short(i);
                match by_url.get(url) {
                    Some(r) => ProxyHealth {
                        wallet_index: i,
                        label,
                        ok: r.ok,
                        latency_ms: r.latency_ms,
                        error: r.error.clone(),
                    },
                    None => ProxyHealth {
                        wallet_index: i,
                        label,
                        ok: false,
                        latency_ms: None,
                        error: Some("probe missing".into()),
                    },
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct ProxyHealth {
    pub wallet_index: usize,
    pub label: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

impl ProxyHealth {
    pub fn short_status(&self) -> String {
        if self.ok {
            match self.latency_ms {
                Some(ms) => format!("OK {}ms", ms),
                None => "OK".into(),
            }
        } else {
            "DOWN".into()
        }
    }
}

#[derive(Debug, Clone)]
struct ProbeResult {
    ok: bool,
    latency_ms: Option<u64>,
    error: Option<String>,
}

async fn probe_direct() -> ProbeResult {
    let started = std::time::Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                ok: false,
                latency_ms: None,
                error: Some(e.to_string()),
            };
        }
    };
    match client.get("https://opensea.io/").send().await {
        Ok(resp) if resp.status().as_u16() < 500 => ProbeResult {
            ok: true,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: None,
        },
        Ok(resp) => ProbeResult {
            ok: false,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: Some(format!("HTTP {}", resp.status())),
        },
        Err(e) => ProbeResult {
            ok: false,
            latency_ms: None,
            error: Some(e.to_string()),
        },
    }
}

async fn probe_proxy_url(proxy_url: &str) -> ProbeResult {
    let started = std::time::Instant::now();
    let proxy = match reqwest::Proxy::all(proxy_url) {
        Ok(p) => p,
        Err(e) => {
            return ProbeResult {
                ok: false,
                latency_ms: None,
                error: Some(format!("bad proxy: {}", e)),
            };
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .proxy(proxy)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                ok: false,
                latency_ms: None,
                error: Some(e.to_string()),
            };
        }
    };
    match client.get("https://opensea.io/").send().await {
        Ok(resp) if resp.status().as_u16() < 500 => ProbeResult {
            ok: true,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: None,
        },
        Ok(resp) => ProbeResult {
            ok: false,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: Some(format!("HTTP {}", resp.status())),
        },
        Err(e) => ProbeResult {
            ok: false,
            latency_ms: None,
            error: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_url() {
        assert_eq!(parse_proxy("http://host:8080").unwrap(), "http://host:8080");
    }

    #[test]
    fn parse_socks5() {
        assert_eq!(parse_proxy("socks5://host:1080").unwrap(), "socks5://host:1080");
    }

    #[test]
    fn parse_user_pass_at_host_port() {
        assert_eq!(
            parse_proxy("user:pass@host:8080").unwrap(),
            "http://user:pass@host:8080"
        );
    }

    #[test]
    fn parse_host_port_user_pass() {
        assert_eq!(
            parse_proxy("host:8080:user:pass").unwrap(),
            "http://user:pass@host:8080"
        );
    }

    #[test]
    fn parse_host_port() {
        assert_eq!(parse_proxy("host:8080").unwrap(), "http://host:8080");
    }

    #[test]
    fn parse_empty_fails() {
        assert!(parse_proxy("").is_err());
    }
}
