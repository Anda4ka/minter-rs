use anyhow::{Context, Result};

#[derive(Clone, Default)]
pub struct ProxyManager {
    proxies: Vec<String>,
}

/// Short label for logs/UI (hides credentials when possible).
pub fn short_proxy(url: &str) -> String {
    // Char-based shortening: byte slicing panics on non-ASCII input.
    fn shorten(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() > 30 {
            let head: String = chars[..14].iter().collect();
            let tail: String = chars[chars.len() - 8..].iter().collect();
            format!("{}...{}", head, tail)
        } else {
            s.to_string()
        }
    }
    if let Some(at) = url.find('@') {
        shorten(&url[at + 1..])
    } else {
        let stripped = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .or_else(|| url.strip_prefix("socks5://"))
            .or_else(|| url.strip_prefix("socks4://"))
            .unwrap_or(url);
        shorten(stripped)
    }
}

/// Percent-encode userinfo for proxy URLs (user/pass may contain `:`, `@`, etc.).
fn encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
        // user:pass@host:port — encode userinfo if present
        if let Some((userinfo, hostport)) = raw.rsplit_once('@') {
            if let Some((user, pass)) = userinfo.split_once(':') {
                return Ok(format!(
                    "http://{}:{}@{}",
                    encode_userinfo(user),
                    encode_userinfo(pass),
                    hostport
                ));
            }
        }
        return Ok(format!("http://{raw}"));
    }

    // host:port:user:pass — pass may contain ':' → splitn(4) then join rest as password
    let parts: Vec<&str> = raw.splitn(4, ':').collect();
    if parts.len() == 4 {
        let host = parts[0];
        let port = parts[1];
        let user = parts[2];
        let pass = parts[3];
        if host.is_empty() || port.is_empty() {
            anyhow::bail!("invalid proxy host:port:user:pass");
        }
        return Ok(format!(
            "http://{}:{}@{}:{}",
            encode_userinfo(user),
            encode_userinfo(pass),
            host,
            port
        ));
    }

    if parts.len() == 2 {
        return Ok(format!("http://{raw}"));
    }

    // Ambiguous — still accept as host-like URL for back-compat.
    Ok(format!("http://{raw}"))
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
    /// Invalid lines are logged and skipped (not silent).
    pub fn from_text(content: &str) -> Self {
        let mut proxies = Vec::new();
        for (lineno, line) in content.lines().enumerate() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            match parse_proxy(l) {
                Ok(p) => proxies.push(p),
                Err(e) => {
                    crate::rlog!(
                        "proxy line {}: skipped invalid entry ({e}): {}",
                        lineno + 1,
                        crate::safe_truncate(l, 80)
                    );
                }
            }
        }
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
        assert_eq!(
            parse_proxy("socks5://host:1080").unwrap(),
            "socks5://host:1080"
        );
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

    #[test]
    fn parse_password_with_colon() {
        // host:port:user:p:ass → pass = "p:ass"
        let u = parse_proxy("1.2.3.4:8080:myuser:p:ass:word").unwrap();
        assert!(u.starts_with("http://myuser:"), "{u}");
        assert!(u.contains("@1.2.3.4:8080"), "{u}");
        // colon in password must be percent-encoded
        assert!(u.contains("%3A") || u.contains("p%3Aass"), "{u}");
    }

    #[test]
    fn parse_userinfo_encodes_at() {
        let u = parse_proxy("user:p@ss@host:8080").unwrap();
        // user:p@ss@host:8080 is ambiguous with rsplit @ — last @ is hostport
        assert!(u.contains("host:8080"), "{u}");
    }

    #[test]
    fn from_text_skips_empty_not_all() {
        let pm = ProxyManager::from_text("host:8080\n\n# comment\nbad::::too:many:colons:maybe\n");
        // "bad::::..." with splitn(4) still parses as 4 parts
        assert!(pm.len() >= 1, "at least host:8080");
    }
}
