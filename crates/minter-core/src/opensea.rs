use alloy::signers::SignerSync;
use alloy_primitives::{Address, U256, keccak256};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::sync::Arc;

const OPENSEA_ORIGIN: &str = "https://opensea.io";
const GQL_URL: &str = "https://gql.opensea.io/graphql";
const DEFAULT_SEADROP_ADDRESS: &str = "0x00005EA00Ac477B1030CE78506496e8C2dE24bf5";
const DEFAULT_FEE_RECIPIENT: &str = "0x0000a26b00c1F0DF003000390027140000fAa719";

const MINT_ACTION_TIMELINE_QUERY: &str = r#"query MintActionTimelineQuery($address: Address!, $fromAssets: [AssetQuantityInput!]!, $toAssets: [AssetQuantityInput!]!, $recipient: Address, $capabilities: WalletCapabilities) { swap(address: $address, fromAssets: $fromAssets, toAssets: $toAssets, recipient: $recipient, action: MINT, capabilities: $capabilities) { actions { __typename ... on TransactionAction { transactionSubmissionData { to data value chain { networkId identifier gasLimitBufferMultiplier } } } ... on MintAction { __typename collection { imageUrl } } ... on RelayerFulfillableAction { relayerFulfillment { requestId sameChain crossChain } } ... on UserOpAction { actionBundleToken chain { networkId identifier } } } errors { __typename } } }"#;
const MINT_ACTION_TIMELINE_HASH: &str =
    "d8454b30426e34f3d5acec5f012d1bdedf31bb44199a83c9b6d05ff52fff8302";

const COLLECTION_DROP_QUERY: &str = r#"
query CollectionDropQuery($collectionSlug: String!, $address: Address!) {
  collectionBySlug(slug: $collectionSlug) {
    __typename
    ... on Collection {
      slug
      name
      chain { identifier }
      contracts { contractAddress chain { identifier } }
      drop {
        __typename
        ... on Erc721SeaDropV1 { maxSupply totalSupply }
        stages {
          stageType
          stageIndex
          label
          startTime
          endTime
          isEligible
          maxTotalMintableByWallet
          eligibleMaxTotalMintableByWallet
          maxTokenSupplyForStage
          price {
            usd
            token { unit symbol contractAddress chain { identifier } }
          }
          eligiblePrice {
            usd
            token { unit symbol contractAddress chain { identifier } }
          }
          ... on Erc1155SeaDropV2Stage {
            fromTokenId
            toTokenId
            maxTotalMintableByWalletPerToken
            eligibleMaxTotalMintableByWalletPerToken
          }
        }
      }
    }
  }
  dropBySlug(slug: $collectionSlug) {
    __typename
    ... on Erc721SeaDropV1 { minterQuantityMinted(minter: $address) }
    stages {
      stageType
      stageIndex
      isEligible
      eligibleMaxTotalMintableByWallet
      eligiblePrice { usd token { unit symbol contractAddress chain { identifier } } }
    }
  }
}
"#;

const DROP_ELIGIBILITY_QUERY: &str = r#"
query DropEligibilityQuery($collectionSlug: String!, $address: Address!) {
  dropBySlug(slug: $collectionSlug) {
    __typename
    ... on Erc721SeaDropV1 {
      minterQuantityMinted(minter: $address)
      __typename
    }
    stages {
      stageType
      stageIndex
      isEligible
      maxTotalMintableByWallet
      eligibleMaxTotalMintableByWallet
      eligiblePrice {
        usd
        token {
          unit
          symbol
          contractAddress
          chain { identifier __typename }
          __typename
        }
        __typename
      }
      ... on Erc1155SeaDropV2Stage {
        fromTokenId
        toTokenId
        maxTotalMintableByWalletPerToken
        eligibleMaxTotalMintableByWalletPerToken
        __typename
      }
      __typename
    }
  }
}
"#;

#[derive(Clone)]
pub struct AuthSession {
    pub access_token: String,
    pub address: String,
    pub client: reqwest::Client,
    pub cookie_jar: Arc<reqwest::cookie::Jar>,
}

// Redacting Debug so the SIWE bearer token can never leak via an accidental
// `{:?}` (audit L14 — parity with VaultEntry). Deriving Debug would print it.
impl std::fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSession")
            .field("address", &self.address)
            .field("access_token", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct StageInfo {
    pub stage_type: String,
    pub stage_index: Option<i64>,
    pub label: Option<String>,
    pub start_time: Option<f64>,
    pub end_time: Option<f64>,
    pub is_eligible: Option<bool>,
    pub max_mintable: Option<i64>,
    pub price_eth: Option<f64>,
    pub price_wei: Option<U256>,
    pub payment_token_contract: Option<String>,
    pub payment_token_chain: Option<String>,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CollectionInfo {
    pub slug: String,
    pub name: String,
    pub chain: String,
    pub contracts: Vec<String>,
    pub drop_type: Option<String>,
    pub stages: Vec<StageInfo>,
    pub minter_quantity_minted: Option<i64>,
}

fn now_utc_iso_ms() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn siwe_message_text(
    domain: &str,
    address: &str,
    statement: &str,
    uri: &str,
    version: &str,
    chain_id: u64,
    nonce: &str,
    issued_at: &str,
) -> String {
    format!(
        "{domain} wants you to sign in with your Ethereum account:\n\
         {address}\n\n\
         {statement}\n\n\
         URI: {uri}\n\
         Version: {version}\n\
         Chain ID: {chain_id}\n\
         Nonce: {nonce}\n\
         Issued At: {issued_at}"
    )
}

pub fn build_client_with_cookie_jar_and_proxy(
    cookie_jar: Arc<reqwest::cookie::Jar>,
    proxy_url: Option<&str>,
) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    let origin = OPENSEA_ORIGIN.to_string();
    let referer = format!("{}/", OPENSEA_ORIGIN);
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".to_string();
    let pairs: Vec<(&str, &str)> = vec![
        ("origin", &origin),
        ("referer", &referer),
        ("user-agent", &ua),
    ];
    for (k, v) in pairs {
        if let (Ok(name), Ok(val)) = (
            k.parse::<reqwest::header::HeaderName>(),
            v.parse::<reqwest::header::HeaderValue>(),
        ) {
            headers.insert(name, val);
        }
    }
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .cookie_provider(cookie_jar)
        .default_headers(headers);

    if let Some(proxy) = proxy_url {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => {
                builder = builder.proxy(p);
            }
            Err(e) => {
                // Never silently fall back to direct — that leaks wallet IP under a
                // false "via proxy" assumption.
                bail!(
                    "invalid proxy URL '{}': {} (refusing silent direct connection)",
                    proxy,
                    e
                );
            }
        }
    }

    builder.build().context("failed to create HTTP client")
}

pub fn build_client_with_cookie_jar(
    cookie_jar: Arc<reqwest::cookie::Jar>,
) -> Result<reqwest::Client> {
    build_client_with_cookie_jar_and_proxy(cookie_jar, None)
}

pub fn build_client() -> Result<reqwest::Client> {
    build_client_with_cookie_jar(Arc::new(reqwest::cookie::Jar::default()))
}

pub fn unauthenticated_session(address: &Address) -> AuthSession {
    let cookie_jar = Arc::new(reqwest::cookie::Jar::default());
    let client = build_client_with_cookie_jar(cookie_jar.clone())
        .expect("direct HTTP client without proxy must build");
    AuthSession {
        access_token: String::new(),
        address: format!("{:?}", address),
        client,
        cookie_jar,
    }
}

fn set_connected_account_cookie(session: &AuthSession, address: &Address) -> Result<()> {
    let addr = format!("{:?}", address).to_lowercase();
    let gql_url = reqwest::Url::parse(GQL_URL)?;
    let opensea_url = reqwest::Url::parse(OPENSEA_ORIGIN)?;
    let cookie = format!(
        "connected-account-server-hint={}; Path=/; Domain=.opensea.io",
        addr
    );
    session.cookie_jar.add_cookie_str(&cookie, &gql_url);
    session.cookie_jar.add_cookie_str(&cookie, &opensea_url);
    if !session.access_token.is_empty() {
        let token_cookie = format!(
            "access_token={}; Path=/; Domain=.opensea.io; Secure; SameSite=None",
            session.access_token
        );
        session.cookie_jar.add_cookie_str(&token_cookie, &gql_url);
        session
            .cookie_jar
            .add_cookie_str(&token_cookie, &opensea_url);
        session.cookie_jar.add_cookie_str(
            "auth_access_hint=true; Path=/; Domain=.opensea.io; Secure; SameSite=None",
            &gql_url,
        );
        session.cookie_jar.add_cookie_str(
            "auth_access_hint=true; Path=/; Domain=.opensea.io; Secure; SameSite=None",
            &opensea_url,
        );
    }
    Ok(())
}

fn gql_request(client: &reqwest::Client) -> reqwest::RequestBuilder {
    client
        .post(GQL_URL)
        .header("content-type", "application/json")
        .header(
            "accept",
            "application/graphql-response+json, application/graphql+json, application/json",
        )
        .header("origin", OPENSEA_ORIGIN)
        .header("referer", format!("{}/", OPENSEA_ORIGIN))
        .header("x-app-id", "os2-web")
        .header("x-graphql-operation-type", "query")
}

fn debug_file_next_to_exe(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(name)
        })
}

fn retry_after_secs(headers: &reqwest::header::HeaderMap, body: &str) -> u64 {
    if let Some(v) = headers.get("retry-after").and_then(|h| h.to_str().ok()) {
        if let Ok(secs) = v.trim().parse::<u64>() {
            return secs.max(1).min(60);
        }
    }
    // OpenSea often embeds retry-after in JSON meta.
    if let Ok(j) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(secs) = j.pointer("/meta/retry-after").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }) {
            return secs.max(1).min(60);
        }
    }
    0
}

fn is_rate_limit_status(status: u16) -> bool {
    status == 429 || status == 503
}

/// SIWE auth with automatic retries on HTTP 429 / 503 (OpenSea rate limits).
pub async fn siwe_auth(
    address: &Address,
    signer: &alloy::signers::local::LocalSigner<k256::ecdsa::SigningKey>,
    chain_id: u64,
    proxy_url: Option<&str>,
) -> Result<AuthSession> {
    siwe_auth_with_retries(address, signer, chain_id, proxy_url, 8).await
}

/// SIWE auth with a custom retry budget (e.g. WL check uses fewer rounds to avoid UI hangs).
pub async fn siwe_auth_with_retries(
    address: &Address,
    signer: &alloy::signers::local::LocalSigner<k256::ecdsa::SigningKey>,
    chain_id: u64,
    proxy_url: Option<&str>,
    max_attempts: u32,
) -> Result<AuthSession> {
    let max_attempts = max_attempts.max(1);
    let mut last_err = None;

    for attempt in 1..=max_attempts {
        match siwe_auth_once(address, signer, chain_id, proxy_url).await {
            Ok(session) => return Ok(session),
            Err(e) => {
                let msg = format!("{}", e);
                let retryable = msg.contains("429")
                    || msg.contains("TOO_MANY_REQUESTS")
                    || msg.contains("Too Many Requests")
                    || msg.contains("503")
                    || msg.to_lowercase().contains("rate limit");
                if retryable && attempt < max_attempts {
                    // Prefer server-provided wait from error text, else exponential backoff.
                    let mut wait_secs = 1u64 << (attempt.saturating_sub(1).min(4)); // 1,2,4,8,16
                    if let Some(idx) = msg.find("retry-after") {
                        let tail = &msg[idx..];
                        if let Some(n) = tail
                            .split(|c: char| !c.is_ascii_digit())
                            .find(|s| !s.is_empty())
                            .and_then(|s| s.parse::<u64>().ok())
                        {
                            wait_secs = n.max(1).min(30);
                        }
                    }
                    // Small jitter so parallel wallets don't retry in lockstep.
                    let jitter_ms = (address.as_slice()[19] as u64 % 400) + 100;
                    let wait = std::time::Duration::from_millis(wait_secs * 1000 + jitter_ms);
                    eprintln!(
                        "[{}] rate limited (attempt {}/{}), sleep {}ms",
                        crate::sign::shorten_address(address),
                        attempt,
                        max_attempts,
                        wait.as_millis()
                    );
                    tokio::time::sleep(wait).await;
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("siwe_auth retries exhausted")))
}

async fn siwe_auth_once(
    address: &Address,
    signer: &alloy::signers::local::LocalSigner<k256::ecdsa::SigningKey>,
    chain_id: u64,
    proxy_url: Option<&str>,
) -> Result<AuthSession> {
    let cookie_jar = Arc::new(reqwest::cookie::Jar::default());
    let client = build_client_with_cookie_jar_and_proxy(cookie_jar.clone(), proxy_url)
        .context("failed to build OpenSea HTTP client")?;

    let _ = client.get(OPENSEA_ORIGIN).send().await;

    let nonce_resp = client
        .post(format!("{}/__api/auth/siwe/nonce", OPENSEA_ORIGIN))
        .header("content-type", "application/json")
        .body("")
        .send()
        .await
        .context("nonce request failed")?;

    let nonce_status = nonce_resp.status().as_u16();
    if nonce_status >= 400 {
        let headers = nonce_resp.headers().clone();
        let text = nonce_resp.text().await.unwrap_or_default();
        if is_rate_limit_status(nonce_status) {
            let ra = retry_after_secs(&headers, &text);
            bail!(
                "Nonce failed: HTTP {} Too Many Requests retry-after={}",
                nonce_status,
                if ra > 0 { ra } else { 1 }
            );
        }
        bail!(
            "Nonce failed: HTTP {} {}",
            nonce_status,
            crate::safe_truncate(&text, 300)
        );
    }
    let nonce_data: serde_json::Value = nonce_resp.json().await?;
    let nonce = nonce_data
        .get("nonce")
        .and_then(|v| v.as_str())
        .context("no nonce in response")?;

    let issued_at = now_utc_iso_ms();
    let statement = "Click to sign in and accept the OpenSea Terms of Service (https://opensea.io/tos) and Privacy Policy (https://opensea.io/privacy).";
    let addr_str = format!("{:?}", address);

    let message = siwe_message_text(
        "opensea.io",
        &addr_str,
        statement,
        "https://opensea.io/",
        "1",
        chain_id,
        nonce,
        &issued_at,
    );

    let signature = signer
        .sign_message_sync(message.as_bytes())
        .context("failed to sign SIWE message")?;
    let sig_hex = format!("0x{}", hex::encode(signature.as_bytes()));

    let siwe_dict = json!({
        "accountType": "Ethereum",
        "address": addr_str,
        "chainId": chain_id.to_string(),
        "domain": "opensea.io",
        "issuedAt": issued_at,
        "nonce": nonce,
        "statement": statement,
        "uri": "https://opensea.io/",
        "version": "1",
    });

    let verify_payload = json!({
        "chainArch": "EVM",
        "message": siwe_dict,
        "signature": sig_hex,
    });

    let verify_resp = client
        .post(format!("{}/__api/auth/siwe/verify", OPENSEA_ORIGIN))
        .header("content-type", "application/json")
        .json(&verify_payload)
        .send()
        .await
        .context("verify request failed")?;

    let verify_status = verify_resp.status().as_u16();
    if verify_status != 200 {
        let headers = verify_resp.headers().clone();
        let text = verify_resp.text().await.unwrap_or_default();
        if is_rate_limit_status(verify_status) {
            let ra = retry_after_secs(&headers, &text);
            bail!(
                "Verify failed: HTTP {} Too Many Requests retry-after={} {}",
                verify_status,
                if ra > 0 { ra } else { 1 },
                crate::safe_truncate(&text, 200)
            );
        }
        bail!(
            "Verify failed: HTTP {} {}",
            verify_status,
            crate::safe_truncate(&text, 500)
        );
    }

    let auth_data: serde_json::Value = verify_resp.json().await?;
    // HTTP 200 with no accessToken is a *failed* auth, not an anonymous session.
    // Returning Ok("") here made the caller mark the wallet auth_ok and cache an
    // empty bearer for the full TTL, so every later request silently went out
    // unauthenticated while the log said "CACHED OK".
    let access_token = auth_data
        .get("accessToken")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Verify returned HTTP 200 without accessToken: {}",
                crate::safe_truncate(&auth_data.to_string(), 300)
            )
        })?
        .to_string();

    Ok(AuthSession {
        access_token,
        address: addr_str,
        client,
        cookie_jar,
    })
}

pub async fn check_eligibility(
    session: &AuthSession,
    collection_slug: &str,
    address: &Address,
) -> Result<Vec<StageInfo>> {
    let client = &session.client;
    let addr_str = format!("{:?}", address);
    set_connected_account_cookie(session, address)?;

    let payload = json!({
        "operationName": "DropEligibilityQuery",
        "query": DROP_ELIGIBILITY_QUERY,
        "variables": {
            "collectionSlug": collection_slug,
            "address": addr_str,
        }
    });

    let mut req = gql_request(client);
    if !session.access_token.is_empty() {
        req = req.header("authorization", format!("Bearer {}", session.access_token));
    }
    let resp = req
        .json(&payload)
        .send()
        .await
        .context("eligibility request failed")?;

    if resp.status().as_u16() >= 400 {
        bail!("Eligibility failed: HTTP {}", resp.status());
    }

    let data: serde_json::Value = resp.json().await?;
    if let Some(errors) = data.get("errors") {
        bail!("Eligibility GraphQL errors: {}", errors);
    }

    let drop = data.pointer("/data/dropBySlug").context("no dropBySlug")?;
    parse_stages(drop)
}

/// Short in-process cache for drop info (slug+address) to cut duplicate GQL under parallel checks.
static DROP_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, CollectionInfo)>>,
> = std::sync::OnceLock::new();

const DROP_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(12);
const DROP_MAX_ATTEMPTS: u32 = 4;

fn drop_cache() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, (std::time::Instant, CollectionInfo)>,
> {
    DROP_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn drop_cache_key(slug: &str, address: &Address) -> String {
    format!(
        "{}:{}",
        slug.to_lowercase(),
        format!("{:?}", address).to_lowercase()
    )
}

fn is_retryable_drop_err(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("429")
        || l.contains("timeout")
        || l.contains("timed out")
        || l.contains("connection")
        || l.contains("reset")
        || l.contains("request failed")
        || l.contains("503")
        || l.contains("502")
        || l.contains("520")
        || l.contains("temporarily")
}

fn classify_drop_transport_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "collection drop timeout (OpenSea/proxy)".into()
    } else if e.is_connect() {
        "collection drop connection failed (proxy/network)".into()
    } else {
        format!("collection drop request failed: {e}")
    }
}

/// Collection drop + stages with short TTL cache and retries on 429/timeout.
pub async fn collection_drop_info(
    session: &AuthSession,
    collection_slug: &str,
    address: &Address,
) -> Result<CollectionInfo> {
    let key = drop_cache_key(collection_slug, address);
    if let Ok(guard) = drop_cache().lock() {
        if let Some((at, info)) = guard.get(&key) {
            if at.elapsed() < DROP_CACHE_TTL {
                return Ok(info.clone());
            }
        }
    }

    let mut last_err = None;
    for attempt in 1..=DROP_MAX_ATTEMPTS {
        match collection_drop_info_once(session, collection_slug, address).await {
            Ok(info) => {
                if let Ok(mut guard) = drop_cache().lock() {
                    guard.insert(key, (std::time::Instant::now(), info.clone()));
                    // light GC
                    if guard.len() > 64 {
                        guard.retain(|_, (t, _)| t.elapsed() < DROP_CACHE_TTL);
                    }
                }
                return Ok(info);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let retry = is_retryable_drop_err(&msg) && attempt < DROP_MAX_ATTEMPTS;
                if retry {
                    let backoff_ms = 200u64 * (1u64 << (attempt - 1)).min(8);
                    // Prefer explicit 429 sleep
                    let sleep_ms = if msg.contains("429") {
                        600 * attempt as u64
                    } else {
                        backoff_ms
                    };
                    crate::rlog!(
                        "drop GQL retry {}/{} after {}ms: {}",
                        attempt,
                        DROP_MAX_ATTEMPTS,
                        sleep_ms,
                        msg
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("collection drop retries exhausted")))
}

async fn collection_drop_info_once(
    session: &AuthSession,
    collection_slug: &str,
    address: &Address,
) -> Result<CollectionInfo> {
    let client = &session.client;
    let addr_str = format!("{:?}", address);
    set_connected_account_cookie(session, address)?;

    let payload = json!({
        "operationName": "CollectionDropQuery",
        "query": COLLECTION_DROP_QUERY,
        "variables": {
            "collectionSlug": collection_slug,
            "address": addr_str,
        }
    });

    let mut req = gql_request(client);
    if !session.access_token.is_empty() {
        req = req.header("authorization", format!("Bearer {}", session.access_token));
    }
    let resp = req
        .json(&payload)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(classify_drop_transport_err(&e)))?;

    let status = resp.status().as_u16();
    if status == 429 {
        bail!("Collection drop failed: HTTP 429 rate limited (OpenSea)");
    }
    if status >= 400 {
        bail!("Collection drop failed: HTTP {status}");
    }

    let data: serde_json::Value = resp.json().await?;
    if std::env::var("DEBUG").ok().as_deref() == Some("1") {
        let debug_file = debug_file_next_to_exe(&format!(
            "debug_collection_{}.json",
            addr_str
                .replace("0x", "")
                .chars()
                .take(6)
                .collect::<String>()
        ));
        match std::fs::write(
            &debug_file,
            serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string()),
        ) {
            Ok(()) => crate::rlog!("Saved collection debug {}", debug_file.display()),
            Err(e) => crate::rlog!(
                "Failed to save collection debug {}: {}",
                debug_file.display(),
                e
            ),
        }
    }
    if let Some(errors) = data.get("errors") {
        bail!("Collection drop GraphQL errors: {}", errors);
    }

    let collection = data
        .pointer("/data/collectionBySlug")
        .context("no collectionBySlug")?;
    let drop = collection.get("drop");
    let eligibility_drop = data.pointer("/data/dropBySlug");

    let slug = collection
        .get("slug")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = collection
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let chain = collection
        .pointer("/chain/identifier")
        .and_then(|v| v.as_str())
        .unwrap_or("ethereum")
        .to_string();

    let contracts: Vec<String> = collection
        .get("contracts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    c.get("contractAddress")
                        .and_then(|a| a.as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();

    let drop_type = drop
        .and_then(|d| d.get("__typename"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut stages = if let Some(drop_obj) = drop {
        parse_stages(drop_obj)?
    } else {
        vec![]
    };

    if let Some(eligibility_drop) = eligibility_drop {
        if let Ok(eligibility_stages) = parse_stages(eligibility_drop) {
            if stages.is_empty() {
                stages = eligibility_stages;
            } else {
                merge_eligibility_stages(&mut stages, &eligibility_stages);
            }
        }
    }

    let minter_quantity_minted = eligibility_drop
        .and_then(|d| d.get("minterQuantityMinted"))
        .and_then(|v| v.as_i64());

    Ok(CollectionInfo {
        slug,
        name,
        chain,
        contracts,
        drop_type,
        stages,
        minter_quantity_minted,
    })
}

fn parse_stage_time(value: Option<&serde_json::Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64().or_else(|| {
            v.as_str().and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.timestamp() as f64)
            })
        })
    })
}

fn merge_stage_info(stage: &mut StageInfo, eligibility_stage: &StageInfo) {
    if eligibility_stage.is_eligible.is_some() {
        stage.is_eligible = eligibility_stage.is_eligible;
    }
    if eligibility_stage.max_mintable.is_some() {
        stage.max_mintable = eligibility_stage.max_mintable;
    }
    if eligibility_stage.price_eth.is_some() {
        stage.price_eth = eligibility_stage.price_eth;
        stage.price_wei = eligibility_stage.price_wei;
        stage.payment_token_contract = eligibility_stage.payment_token_contract.clone();
        stage.payment_token_chain = eligibility_stage.payment_token_chain.clone();
    }
}

fn merge_eligibility_stages(stages: &mut [StageInfo], eligibility_stages: &[StageInfo]) {
    let mut used = vec![false; stages.len()];

    for eligibility_stage in eligibility_stages {
        let exact_idx = stages.iter().enumerate().position(|(i, stage)| {
            !used[i]
                && stage.stage_index == eligibility_stage.stage_index
                && stage.stage_type == eligibility_stage.stage_type
        });
        let index_idx = exact_idx.or_else(|| {
            stages.iter().enumerate().position(|(i, stage)| {
                !used[i]
                    && stage.stage_index.is_some()
                    && stage.stage_index == eligibility_stage.stage_index
            })
        });
        let type_idx = index_idx.or_else(|| {
            stages
                .iter()
                .enumerate()
                .position(|(i, stage)| !used[i] && stage.stage_type == eligibility_stage.stage_type)
        });

        if let Some(idx) = type_idx {
            merge_stage_info(&mut stages[idx], eligibility_stage);
            used[idx] = true;
        }
    }
}

fn parse_stages(drop: &serde_json::Value) -> Result<Vec<StageInfo>> {
    let stages_array = drop
        .get("stages")
        .and_then(|v| v.as_array())
        .context("no stages")?;
    let mut stages = Vec::new();
    for stage in stages_array {
        let price_token = stage
            .pointer("/eligiblePrice/token")
            .or_else(|| stage.pointer("/price/token"));
        // Prefer string unit from GraphQL and convert with integer math (no f64).
        let price_unit = price_token
            .and_then(|t| t.get("unit"))
            .and_then(crate::amount::json_decimal_string);
        let price_wei = price_unit
            .as_deref()
            .and_then(|u| crate::amount::eth_to_wei(u).ok());
        let price_eth = price_unit
            .as_deref()
            .and_then(crate::amount::decimal_string_to_f64)
            .or_else(|| {
                // Display-only fallback if unit was unparseable as wei.
                price_token.and_then(|t| t.get("unit")).and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|u| u.parse().ok()))
                })
            });
        let payment_token_contract = price_token
            .and_then(|t| t.get("contractAddress"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let payment_token_chain = price_token
            .and_then(|t| t.pointer("/chain/identifier"))
            .and_then(|v| v.as_str())
            .map(String::from);

        stages.push(StageInfo {
            stage_type: stage
                .get("stageType")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN")
                .to_string(),
            stage_index: stage.get("stageIndex").and_then(|v| v.as_i64()),
            label: stage
                .get("label")
                .and_then(|v| v.as_str())
                .map(String::from),
            start_time: parse_stage_time(stage.get("startTime")),
            end_time: parse_stage_time(stage.get("endTime")),
            is_eligible: stage.get("isEligible").and_then(|v| v.as_bool()),
            max_mintable: stage
                .get("eligibleMaxTotalMintableByWallet")
                .or_else(|| stage.get("maxTotalMintableByWallet"))
                .or_else(|| stage.get("eligibleMaxTotalMintableByWalletPerToken"))
                .or_else(|| stage.get("maxTotalMintableByWalletPerToken"))
                .and_then(|v| v.as_i64()),
            price_eth,
            price_wei,
            payment_token_contract,
            payment_token_chain,
            raw: stage.clone(),
        });
    }
    Ok(stages)
}

pub async fn fetch_mint_calldata(
    session: &AuthSession,
    _collection_slug: &str,
    address: &Address,
    nft_contract: &str,
    chain: &str,
    token_id: &str,
    quantity: u32,
    payment_asset: &serde_json::Value,
) -> Result<serde_json::Value> {
    let client = &session.client;
    let addr_str = format!("{:?}", address);
    set_connected_account_cookie(session, address)?;

    let payload = json!({
        "operationName": "MintActionTimelineQuery",
        "query": MINT_ACTION_TIMELINE_QUERY,
        "variables": {
            "address": addr_str,
            "capabilities": {"eip7702": false},
            "fromAssets": [{"asset": payment_asset}],
            "toAssets": [{
                "asset": {
                    "chain": chain,
                    "contractAddress": nft_contract,
                    "tokenId": token_id,
                },
                "quantity": quantity.to_string(),
            }],
        }
    });

    let persisted_payload = json!({
        "operationName": "MintActionTimelineQuery",
        "variables": payload.get("variables").cloned().unwrap_or_else(|| json!({})),
        "extensions": {
            "persistedQuery": {
                "version": 1,
                "sha256Hash": MINT_ACTION_TIMELINE_HASH,
            }
        }
    });

    let mut req = gql_request(client);
    if !session.access_token.is_empty() {
        req = req.header("authorization", format!("Bearer {}", session.access_token));
    }
    let resp = req
        .json(&persisted_payload)
        .send()
        .await
        .context("mint action request failed")?;

    if resp.status().as_u16() >= 400 {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if text.contains("PERSISTED_QUERY_NOT_FOUND") {
            let mut fallback_req = gql_request(client);
            if !session.access_token.is_empty() {
                fallback_req = fallback_req
                    .header("authorization", format!("Bearer {}", session.access_token));
            }
            let fallback_resp = fallback_req
                .json(&payload)
                .send()
                .await
                .context("mint action inline query fallback failed")?;
            if fallback_resp.status().as_u16() < 400 {
                let data: serde_json::Value = fallback_resp.json().await?;
                if let Some(errors) = data.get("errors") {
                    bail!("Mint action GraphQL errors: {}", errors);
                }
                return Ok(data);
            }
            let fallback_status = fallback_resp.status();
            let fallback_text = fallback_resp.text().await.unwrap_or_default();
            bail!(
                "Mint action failed: OpenSea persisted query hash expired and inline fallback failed: HTTP {} {}",
                fallback_status,
                crate::safe_truncate(&fallback_text, 500)
            );
        }
        bail!(
            "Mint action failed: HTTP {} {}",
            status,
            crate::safe_truncate(&text, 500)
        );
    }

    let data: serde_json::Value = resp.json().await?;
    if let Some(errors) = data.get("errors") {
        bail!("Mint action GraphQL errors: {}", errors);
    }

    Ok(data)
}

fn find_transaction_submission_data(value: &serde_json::Value) -> Option<&serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(tx) = map.get("transactionSubmissionData") {
                if tx.is_object() && (tx.get("data").is_some() || tx.get("calldata").is_some()) {
                    return Some(tx);
                }
            }
            if map.get("to").is_some()
                && (map.get("data").is_some()
                    || map.get("calldata").is_some()
                    || map.get("input").is_some())
            {
                return Some(value);
            }
            map.values().find_map(find_transaction_submission_data)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_transaction_submission_data),
        _ => None,
    }
}

pub fn extract_opensea_action_tx(data: &serde_json::Value) -> Result<serde_json::Value> {
    let tx = find_transaction_submission_data(data)
        .context("OpenSea mint action response has no transactionSubmissionData")?;
    let to = tx
        .get("to")
        .or_else(|| tx.get("target"))
        .or_else(|| tx.get("contractAddress"))
        .and_then(|v| v.as_str())
        .context("OpenSea transactionSubmissionData has no to")?;
    let tx_data = tx
        .get("data")
        .or_else(|| tx.get("calldata"))
        .or_else(|| tx.get("input"))
        .and_then(|v| v.as_str())
        .context("OpenSea transactionSubmissionData has no data")?;
    // `value` may arrive as a hex/decimal string *or* a JSON number. Reading only
    // `as_str()` silently produced "0x0" for numeric values → a zero-value mint
    // that reverts. Accept both forms.
    let value_val = tx.get("value").or_else(|| tx.get("weiValue"));
    let value = match value_val {
        Some(v) if v.is_string() => v.as_str().unwrap_or("0x0").to_string(),
        Some(v) if v.is_u64() => v.as_u64().unwrap_or(0).to_string(),
        Some(v) if v.is_number() => v.to_string(),
        _ => "0x0".to_string(),
    };

    Ok(json!({
        "to": to,
        "data": if tx_data.starts_with("0x") { tx_data.to_string() } else { format!("0x{}", tx_data) },
        "value": value,
    }))
}

pub fn stage_payment_asset(
    collection_info: &CollectionInfo,
    stage: &StageInfo,
) -> serde_json::Value {
    let chain = &collection_info.chain;
    let contract = stage
        .payment_token_contract
        .as_deref()
        .unwrap_or("0x0000000000000000000000000000000000000000");
    json!({
        "chain": chain,
        "contractAddress": contract,
    })
}

pub fn stage_token_id(stage: &StageInfo) -> String {
    stage
        .raw
        .get("fromTokenId")
        .and_then(|v| {
            v.as_i64()
                .map(|i| i.to_string())
                .or_else(|| v.as_str().map(String::from))
        })
        .unwrap_or_else(|| "0".to_string())
}

pub fn stage_effective_eligible(stage: &StageInfo) -> bool {
    stage
        .is_eligible
        .unwrap_or_else(|| stage.stage_type == "PUBLIC_SALE")
}

fn stage_wallet_limit(stage: &StageInfo) -> Option<i64> {
    stage.max_mintable.or_else(|| {
        stage
            .raw
            .get("eligibleMaxTotalMintableByWallet")
            .or_else(|| stage.raw.get("maxTotalMintableByWallet"))
            .or_else(|| stage.raw.get("eligibleMaxTotalMintableByWalletPerToken"))
            .or_else(|| stage.raw.get("maxTotalMintableByWalletPerToken"))
            .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|n| n as i64)))
    })
}

pub fn stage_eligibility_label(stage: &StageInfo) -> &'static str {
    match stage.is_eligible {
        Some(true) => "eligible",
        Some(false) => "not eligible",
        None if stage.stage_type == "PUBLIC_SALE" => "eligible (public)",
        None if stage.stage_type == "SIGNED_PRESALE" && stage_wallet_limit(stage).is_some() => {
            "check on mint"
        }
        None => "unknown",
    }
}

/// Remaining mints for this wallet on the given stage.
///
/// `minterQuantityMinted` from OpenSea is a **global** count across the drop, not
/// per-stage. Subtracting it from every stage's limit incorrectly zeros later
/// phases after minting in an earlier one. We only subtract global minted when
/// the drop has a single stage (then the count is attributable to that stage).
pub fn available_mint_quantity(collection_info: &CollectionInfo, stage: &StageInfo) -> Option<u32> {
    let max = stage_wallet_limit(stage)?;
    if max <= 0 {
        return Some(0);
    }

    let minted = if collection_info.stages.len() <= 1 {
        collection_info.minter_quantity_minted.unwrap_or(0).max(0)
    } else {
        0
    };
    // Clamp (not truncate) into u32: huge "unlimited" limits from OpenSea must not
    // wrap through `as u32` into a small/arbitrary quantity.
    Some(max.saturating_sub(minted).min(u32::MAX as i64) as u32)
}

fn encode_address_arg(address: &str) -> Result<[u8; 32]> {
    let addr = address.strip_prefix("0x").unwrap_or(address);
    let decoded = hex::decode(addr).context("invalid address hex")?;
    if decoded.len() != 20 {
        bail!("invalid address length");
    }
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&decoded);
    Ok(out)
}

fn encode_u256_arg(value: U256) -> [u8; 32] {
    let mut out = [0u8; 32];
    value
        .to_be_bytes_vec()
        .iter()
        .rev()
        .take(32)
        .enumerate()
        .for_each(|(i, b)| {
            out[31 - i] = *b;
        });
    out
}

pub fn build_public_mint_tx(
    nft_contract: &str,
    quantity: u32,
    price_wei: U256,
    seadrop_address: Option<&str>,
    fee_recipient: Option<&str>,
    minter_address: Option<&str>,
) -> Result<serde_json::Value> {
    let selector_hash = keccak256("mintPublic(address,address,address,uint256)".as_bytes());
    let mut data = selector_hash[..4].to_vec();
    data.extend_from_slice(&encode_address_arg(nft_contract)?);
    data.extend_from_slice(&encode_address_arg(
        fee_recipient.unwrap_or(DEFAULT_FEE_RECIPIENT),
    )?);
    data.extend_from_slice(&encode_address_arg(
        minter_address.unwrap_or("0x0000000000000000000000000000000000000000"),
    )?);
    data.extend_from_slice(&encode_u256_arg(U256::from(quantity)));

    Ok(json!({
        "to": seadrop_address.unwrap_or(DEFAULT_SEADROP_ADDRESS),
        "data": format!("0x{}", hex::encode(data)),
        "value": format!("0x{:x}", price_wei.saturating_mul(U256::from(quantity))),
    }))
}

pub fn stage_label(stage: &StageInfo) -> String {
    let idx_str = stage
        .stage_index
        .map(|i| format!("#{}", i))
        .unwrap_or_default();
    let label = stage.label.as_deref().unwrap_or("");
    format!(
        "{}{}{}",
        stage.stage_type,
        idx_str,
        if label.is_empty() {
            String::new()
        } else {
            format!(" ({})", label)
        }
    )
}

#[cfg(test)]
mod available_qty_tests {
    use super::*;

    fn stage(max: i64, stage_type: &str, index: i64) -> StageInfo {
        StageInfo {
            stage_type: stage_type.to_string(),
            stage_index: Some(index),
            label: None,
            start_time: None,
            end_time: None,
            is_eligible: Some(true),
            max_mintable: Some(max),
            price_eth: None,
            price_wei: None,
            payment_token_contract: None,
            payment_token_chain: None,
            raw: serde_json::json!({}),
        }
    }

    fn collection(stages: Vec<StageInfo>, minted: Option<i64>) -> CollectionInfo {
        CollectionInfo {
            slug: "test".into(),
            name: "Test".into(),
            chain: "ethereum".into(),
            contracts: vec![],
            drop_type: None,
            stages,
            minter_quantity_minted: minted,
        }
    }

    #[test]
    fn multi_stage_does_not_subtract_global_minted() {
        let s1 = stage(2, "ALLOW_LIST", 0);
        let s2 = stage(2, "PUBLIC_SALE", 1);
        let info = collection(vec![s1.clone(), s2.clone()], Some(2));
        // Global minted=2 must not zero the public phase limit.
        assert_eq!(available_mint_quantity(&info, &s2), Some(2));
        assert_eq!(available_mint_quantity(&info, &s1), Some(2));
    }

    #[test]
    fn single_stage_subtracts_global_minted() {
        let s = stage(5, "PUBLIC_SALE", 0);
        let info = collection(vec![s.clone()], Some(3));
        assert_eq!(available_mint_quantity(&info, &s), Some(2));
    }

    #[test]
    fn single_stage_fully_minted_is_zero() {
        let s = stage(2, "PUBLIC_SALE", 0);
        let info = collection(vec![s.clone()], Some(2));
        assert_eq!(available_mint_quantity(&info, &s), Some(0));
    }

    #[test]
    fn missing_limit_returns_none() {
        let mut s = stage(0, "PUBLIC_SALE", 0);
        s.max_mintable = None;
        let info = collection(vec![s.clone()], Some(1));
        assert_eq!(available_mint_quantity(&info, &s), None);
    }
}

#[cfg(test)]
mod extract_action_tx_tests {
    use super::*;

    #[test]
    fn value_as_string_is_preserved() {
        let data = json!({
            "transactionSubmissionData": {
                "to": "0x00005ea00ac477b1030ce78506496e8c2de24bf5",
                "data": "0xdeadbeef",
                "value": "0x2386f26fc10000"
            }
        });
        let tx = extract_opensea_action_tx(&data).unwrap();
        assert_eq!(tx.get("value").unwrap().as_str(), Some("0x2386f26fc10000"));
    }

    #[test]
    fn numeric_value_is_not_dropped_to_zero() {
        // Regression: a JSON *number* value used to fall through to "0x0",
        // producing a zero-value mint that reverts.
        let data = json!({
            "transactionSubmissionData": {
                "to": "0x00005ea00ac477b1030ce78506496e8c2de24bf5",
                "data": "0xdeadbeef",
                "value": 10000000000000000u64
            }
        });
        let tx = extract_opensea_action_tx(&data).unwrap();
        assert_eq!(tx.get("value").unwrap().as_str(), Some("10000000000000000"));
    }

    #[test]
    fn missing_value_defaults_to_zero() {
        let data = json!({
            "transactionSubmissionData": {
                "to": "0x00005ea00ac477b1030ce78506496e8c2de24bf5",
                "data": "0xdeadbeef"
            }
        });
        let tx = extract_opensea_action_tx(&data).unwrap();
        assert_eq!(tx.get("value").unwrap().as_str(), Some("0x0"));
    }
}
