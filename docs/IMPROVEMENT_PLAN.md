# MINTER — план улучшений (реализация)

Документ описывает **детальный план реализации** выбранных улучшений (features).

> **Runtime-баги P0–P2 закрыты** — см. `docs/BUGFIX_PLAN.md` и `docs/CODE_AUDIT.md` (2026-07).  
> **Оставшиеся риски (M*/L*):** `docs/RISK_MITIGATION_PLAN.md` (L5=#10, L6=#4).  
> **Доставка:** только `Public\minter-desktop.exe` (`scripts/package-public.ps1`).  
> **Архитектура:** `docs/ARCHITECTURE.md`.

| # | Тема | Статус |
|---|------|--------|
| 4 | Event-driven open (hybrid open signals) | planned |
| 7 | Smart funding pipeline | planned |
| 9 | Observability / latency budget | planned |
| 10 | Hot-path tests + ABI completeness | planned (partial: 99 unit tests already) |

**Стек:** `minter-core` (Rust) + `minter-desktop` (Tauri 2 + vanilla UI).  
**Принцип:** локально, без телеметрии, burner wallets only. Не ломать текущий default-поведение (wall clock / pre-sign race).

---

## Порядок внедрения

```
PR0  #10 partial  — unit tests + ABI arrays (safety net)
PR1  #7           — funding plan + UI + optional variable disperse
PR2  #9           — metrics collector + export + history fields
PR3  #4           — hybrid open (OpenSea + Raw MintBay)
PR4  #10 full     — mock RPC integration + ABI tuples
```

Почему так:

1. Тесты/ABI ловят регрессии до правок hot path.
2. Funding — UX, не трогает sniper timing.
3. Metrics — baseline «до/после» hybrid open.
4. Open hybrid — самый чувствительный к latency кусок, идёт под метриками.
5. Integration tests закрепляют 4/7.

---

## Общие договорённости

### Default behaviour

- **Open mode default:** `clock` (как сейчас) — zero surprise для существующих tasks.
- **Hybrid / event:** opt-in в task / raw settings.
- **Funding / metrics:** additive API + UI; старые `results/*.json` остаются валидными.

### Сериализация

- JSON: `camelCase` (как `MintEvent`, `MintOptions`, export).
- Новые поля: `#[serde(default)]` + `skip_serializing_if` где уместно.

### Критерии готовности (Definition of Done)

- `cargo test -p minter-core` green  
- `cargo check -p minter-core -p minter-desktop` green  
- dry-run mint / raw sniper не регрессируют  
- i18n EN + RU для новых строк  
- краткая заметка в `Public/ИНСТРУКЦИЯ.md` (по мере merge)

---

# 4. Event-driven open

## 4.1 Цель

После **Start** бот **сам** ждёт открытия фазы и стреляет по **первому надёжному** сигналу.  
Не ручной fire, не замена clock — **гибрид** clock ∥ on-chain ∥ API.

## 4.2 Текущее состояние

| Path | Файл | Поведение |
|------|------|-----------|
| OpenSea mint | `crates/minter-core/src/mint.rs` (~1281–1504) | wait loop: `start_ts - wall_ts`; fire только по wall clock; prefetch T−5s; nonce refresh T−2s |
| Raw sniper | `crates/minter-core/src/raw_sniper.rs` | pre-sign @ `at_time − 5s` → clock fire → blast; **нет** gate `getMintStatus` на T0 |
| MintBay status | `raw_sniper.rs` `fetch_mintbay_status` / `is_public_open` | уже есть; используется в `api::probe_raw`, **не** в fire path |

Комментарий в `mint.rs`: **не** ждать `block.timestamp` (lag ~1 block). Сохраняем.

## 4.3 UX: автоматически?

**Да.** Оператор:

1. Один раз выбирает `openMode` в task / raw form.
2. Жмёт Start.
3. Бот: prep → wait (signals) → fire → confirm.

Опционально (nice-to-have, не v1): кнопка **Force fire**.  
Обязательно: **Cancel** (уже есть).

### Режимы

| Mode | Поведение | Default |
|------|-----------|---------|
| `clock` | только wall clock (`at_time` / `stage.start_time`) | **да** |
| `hybrid` | clock **или** on-chain/API (что раньше, с anti-FP правилами) | opt-in |
| `event` | только on-chain/API; clock = deadline/timeout banner | opt-in |

Доп. поля:

- `earlyWindowSecs` (default `2`) — насколько раньше clock можно принять on-chain/API open.
- `openConfirmPolls` (default `2`) — сколько подряд успешных probe нужно для non-clock open.

## 4.4 Модель (core)

Новый модуль: `crates/minter-core/src/open_signal.rs`

```rust
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OpenMode {
    #[default]
    Clock,
    Hybrid,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSignalKind {
    Clock,
    OnChain,
    Api,
}

#[derive(Debug, Clone)]
pub struct OpenWaitConfig {
    pub mode: OpenMode,
    /// Unix seconds when phase is expected to open (None = unknown / event-only).
    pub open_ts: Option<i64>,
    pub early_window_secs: i64,      // default 2
    pub open_confirm_polls: u32,     // default 2
    pub timeout_secs: u64,           // hard stop
}

#[derive(Debug, Clone)]
pub struct OpenDecision {
    pub kind: OpenSignalKind,
    pub wall_unix_ms: i64,
    pub open_ts: Option<i64>,
    pub fire_lag_ms: i64, // wall - open_ts*1000 (if known)
}

/// Pure policy: given clock_ready + consecutive open probes + mode → fire?
pub fn should_fire(
    cfg: &OpenWaitConfig,
    now_unix: i64,
    consecutive_open: u32,
) -> Option<OpenSignalKind> { /* … */ }
```

### Политика `should_fire` (pure, unit-tested)

```
clock_ready = open_ts.map(|t| now >= t).unwrap_or(false)
probe_ready = consecutive_open >= open_confirm_polls
early_ok    = open_ts.map(|t| now >= t - early_window_secs).unwrap_or(true)

match mode:
  Clock  => clock_ready → Clock
  Event  => probe_ready && early_ok → OnChain/Api
  Hybrid =>
      if clock_ready → Clock
      else if probe_ready && early_ok → OnChain/Api
      else None
```

**Anti false-positive:** probe open при `now < open_ts − early_window` → **игнор** (log warning).

**Clock hit при closed on-chain:** в Hybrid/Clock — **fire всё равно** (как сейчас; chain lag нормален).

## 4.5 OpenSea path — реализация

**Файл:** `mint.rs` wait loop.

### Изменения loop

Сейчас: break только при `left <= 0`.

Добавить:

1. Прочитать `opts.open_mode` (default Clock).
2. При `Hybrid` / `Event` и `left <= 15` (или всегда в Event):  
   - poll open detector (см. ниже)  
   - увеличить `consecutive_open` / сбросить  
3. `should_fire(...)` → break + log `OPEN_SIGNAL=…`.
4. Prefetch / nonce refresh: оставить T−5 / T−2 **относительно open_ts**;  
   если event-open раньше clock и prefetch не стартовал — workers сами fetch (уже fallback).

### Open detectors (OpenSea)

| Signal | Как | Частота | v1? |
|--------|-----|---------|-----|
| Clock | `Utc::now() >= start_ts` | tight sleep (уже) | да |
| Api | soft re-check stage via cached session (drop info / stage active) | 1–2 s | да (best-effort) |
| OnChain | optional SeaDrop-related eth_call if trivial; иначе skip | 100–200 ms near open | v1.1 |

v1 минимум: **Clock + Api soft** (Api не блокирует, только ускоряет hybrid).  
On-chain для SeaDrop сложнее (calldata/proof) — не gate без sim.

### MintOptions / tasks

`api.rs` `MintOptions`:

```rust
pub open_mode: Option<String>,        // "clock" | "hybrid" | "event"
pub early_window_secs: Option<i64>,
pub open_confirm_polls: Option<u32>,
```

Tasks JSON: те же поля в task object (UI save/load).

### UI

- Tasks form: select **Open mode** + help tooltip.  
- Phase banner: `Waiting (hybrid)…` / `OPEN via api (+0.3s)`.  
- i18n keys: `tasks.openMode.*`

## 4.6 Raw sniper path — реализация

**Файл:** `raw_sniper.rs`

Критично: **не** ставить `getMintStatus` **до** pre-sign как gate на T0 value path.  
Event race — **после** pre-sign.

```
wait until prep (at_time − PREP_LEAD)     # clock, as now
resolve value + PRE-SIGN all wallets
wait FIRE:
  A) clock: now >= at_time          (if Some)
  B) MintBay: is_public_open()      (if preset MintBayPublic && mode hybrid|event)
first success → blast
```

### Config

`RawSniperConfig` / `RawSniperInput`:

```rust
pub open_mode: OpenMode,           // default Clock
pub early_window_secs: i64,
pub open_confirm_polls: u32,
```

### MintBay poll

- Reuse `fetch_mintbay_status` + `is_public_open(wall_now)`.
- Interval: 200 ms far / 50 ms near `at_time` or always 50–100 ms in Event without `at_time`.
- `consecutive_open` policy as above.
- SimpleMint / Custom: Event без user-defined view → только clock (или immediate); document in UI.

### UI Raw

- Select open mode (default Clock).  
- Probe button уже показывает OPEN — link help: «Hybrid uses same status as Probe».

## 4.7 Метрики (стык с #9)

При fire писать:

```json
"openSignal": { "kind": "clock|onChain|api", "fireLagMs": 12, "openTs": 1700000000 }
```

## 4.8 Тесты (#10)

- `should_fire` matrix (mode × clock × probes × early window).  
- MintBay `is_public_open` boundaries (уже частично есть).  
- Integration mock: status flips open before clock → hybrid fires (raw).

## 4.9 Файлы

| Файл | Действие |
|------|----------|
| `minter-core/src/open_signal.rs` | **new** |
| `minter-core/src/lib.rs` | `pub mod open_signal` |
| `minter-core/src/mint.rs` | hybrid wait loop |
| `minter-core/src/raw_sniper.rs` | post-presign open race |
| `minter-core/src/api.rs` | options fields, parse mode |
| `minter-desktop/src-tauri/src/lib.rs` | pass-through if needed |
| `minter-desktop/ui/app.js` | task/raw fields |
| `minter-desktop/ui/index.html` | select |
| `minter-desktop/ui/i18n.js` | EN/RU |

## 4.10 Оценка

- Core policy + tests: 0.5–1 day  
- OpenSea loop + UI: 1–1.5 day  
- Raw MintBay race + UI: 1 day  
- **Итого:** ~2.5–4 days  

**Риск:** medium (timing). Mitigation: default `clock`, feature flag via mode only.

---

# 7. Smart funding pipeline

## 7.1 Цель

Связка: **цена mint + gas → per-wallet need/deficit → plan → disperse → re-check → mint**.  
Убрать ручную арифметику «сколько лить на burner».

## 7.2 Текущее состояние

| Кусок | Где | Ограничение |
|-------|-----|-------------|
| Balance gate | `mint.rs` ~1149–1223 | skip low balance; `need = price×qty + gas_est` |
| Disperse | `disperse.rs` + `api::disperse` | fixed `amount` × N destinations |
| WL check | `api::check_eligibility_wallets` | не связан с disperse |
| UI Disperse | `app.js` | manual amount |

## 7.3 UX flow

```
WL Check / Tasks / Disperse
        │
        ▼
 [ Plan funding ]
        │
        ▼
 Table: address | have | need mint | need gas | total | deficit | OK?
 Summary: recipients, amount mode, source must hold X ETH
        │
        ├─ Dry-run disperse
        └─ Live disperse
        │
        ▼
 Auto refresh balances → Start mint when all OK
```

## 7.4 Core API

Новый модуль: `crates/minter-core/src/funding.rs`

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FundingMode {
    /// All wallets topped to the same target balance (need_total).
    #[default]
    EqualTarget,
    /// Each wallet receives exactly its deficit (v1.1).
    ExactDeficit,
}

#[derive(Debug, Clone)]
pub struct FundingPlanInput {
    pub chain: String,
    pub mint_price_wei: U256,
    pub quantity_default: u64,
    pub quantities: HashMap<Address, u64>, // optional per-wallet
    pub gas_limit: u64,
    pub max_fee_per_gas: U256,             // from fee_history + multipliers
    pub gas_buffer_bps: u32,               // default 1000 = +10%
    pub wallets: Vec<Address>,
    pub balances: HashMap<Address, U256>,
    pub mode: FundingMode,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingWalletRow {
    pub address: String,
    pub have_wei: String,
    pub have_eth: String,
    pub need_mint_wei: String,
    pub need_gas_wei: String,
    pub need_total_wei: String,
    pub need_total_eth: String,
    pub deficit_wei: String,
    pub deficit_eth: String,
    pub ok: bool,
    pub quantity: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingPlan {
    pub chain: String,
    pub mode: FundingMode,
    pub rows: Vec<FundingWalletRow>,
    pub recipients: Vec<String>,           // deficit > 0
    /// EqualTarget: single amount to send each recipient
    pub amount_each_wei: Option<String>,
    pub amount_each_eth: Option<String>,
    /// ExactDeficit: parallel to recipients
    pub amounts_wei: Option<Vec<String>>,
    pub source_value_wei: String,          // sum to send
    pub source_gas_wei: String,            // estimate for N txs
    pub source_total_need_wei: String,
    pub source_total_need_eth: String,
    pub ready_count: usize,
    pub need_topup_count: usize,
}

pub fn build_funding_plan(input: &FundingPlanInput) -> FundingPlan { /* pure */ }

fn need_total(price: U256, qty: u64, gas_limit: u64, max_fee: U256, buffer_bps: u32) -> U256 {
    let mint = price.saturating_mul(U256::from(qty.max(1)));
    let gas = U256::from(gas_limit).saturating_mul(max_fee);
    let gas_buf = gas.saturating_mul(U256::from(10_000u32 + buffer_bps)) / U256::from(10_000u32);
    mint.saturating_add(gas_buf)
}
```

### EqualTarget (v1)

Для каждого wallet: `target = need_total(…)` (может отличаться если qty разный — тогда amount_each **не** single; либо force same qty).

Упрощение v1:

- UI: одна qty на всех (как mint task).  
- `amount_each = max(deficits)` **или** `amount_each = target` with «send only if below target» via ExactDeficit.

Практичнее для оператора:

- **Recommended v1:** ExactDeficit sequential в `disperse` variable amounts;  
  если variable сложно — EqualTarget где `amount_each = ceil(max deficit)` only to wallets with deficit (overfund slightly OK for burners).

**Решение v1:**  

1. `build_funding_plan` считает deficit per wallet.  
2. Disperse: **fixed amount** = `max(deficit_i)` among recipients **или** user-edited field prefilled.  
3. v1.1: `run_disperse_variable`.

### Session API

`api.rs`:

```rust
pub async fn plan_funding(
    &self,
    chain: &str,
    addresses: Vec<String>,          // empty = all vault
    mint_price_eth: &str,            // or wei string
    quantity: u32,
    gas_limit: Option<u64>,
    gas_buffer_bps: Option<u32>,
    eligible_only: bool,             // filter via last eligibility? or pass addresses from UI
) -> Result<FundingPlan>
```

Реализация:

1. Resolve RPC, `fee_history`, `calculate_fees`.  
2. `wallet_balances` for addresses.  
3. `build_funding_plan`.  
4. Не шлёт tx.

```rust
pub async fn disperse_from_plan(
    &self,
    chain: &str,
    from_address: &str,
    plan: FundingPlan,   // or recipients + amount_each
    dry_run: bool,
) -> Result<Vec<SweepResultRow>>
```

v1: map plan → existing `disperse(amount_each)`.

### Optional v1.1 disperse variable

`disperse.rs`:

```rust
pub async fn run_disperse_variable(
    from: &Signer,
    pairs: &[(Address, U256)],  // to, amount
    rpc: &RpcClient,
    gas: &GasParams,
    dry_run: bool,
) -> Vec<MintResult>
```

## 7.5 Источники цены

| Context | Price source |
|---------|--------------|
| OpenSea task | `stage.price_wei` from drop phases |
| Raw MintBay | `fetch_mintbay_status` → `mint_value(qty)` / per-nft |
| Manual | user ETH field in Plan dialog |

UI: prefill from current task / probe when possible.

## 7.6 Связка WL

На странице WL после `check_eligibility_wallets`:

- кнопка **Fund eligible** → addresses where eligible → Plan funding.

## 7.7 UI

### Disperse page (primary)

- Block **Smart plan**: chain, price, qty, buffer %, wallet filter (all / group / selected / paste).  
- Button **Calculate**.  
- Table of rows + summary.  
- Prefill amount input + selected destinations.  
- Existing Dry-run / Live buttons.

### Tasks page (secondary)

- Link «Plan funding for this task» → switch to Disperse with prefilled price/qty/chain.

### i18n

`funding.*` keys EN/RU.

## 7.8 Файлы

| Файл | Действие |
|------|----------|
| `minter-core/src/funding.rs` | **new** |
| `minter-core/src/lib.rs` | mod + re-export |
| `minter-core/src/disperse.rs` | optional variable amounts |
| `minter-core/src/api.rs` | `plan_funding`, `disperse_from_plan` |
| `minter-desktop/src-tauri/src/lib.rs` | Tauri commands |
| `ui/index.html` | plan form + table |
| `ui/app.js` | handlers |
| `ui/i18n.js` | strings |
| `ui/styles.css` | table polish if needed |

## 7.9 Тесты

- `need_total` math (buffer bps).  
- zero deficit → recipients empty.  
- underfunded all → all in recipients.  
- EqualTarget amount_each = max deficit.  
- quantity map per wallet.

## 7.10 Оценка

- Pure plan + tests: 0.5–1 day  
- API + balances wire: 0.5 day  
- UI Disperse + i18n: 1 day  
- WL/Tasks links: 0.5 day  
- Variable disperse (optional): +1 day  
- **Итого v1:** ~2.5–3.5 days  

**Риск:** low (off hot path).

---

# 9. Observability / latency budget

## 9.1 Цель

Структурированные метрики run: **где теряются ms**, success/fail breakdown, сравнение runs.  
Без cloud, только local JSON + UI history.

## 9.2 Текущее состояние

| Артефакт | Содержимое |
|----------|------------|
| `MintEvent` / `FileTeeReporter` | text logs with timestamps |
| `MintRunExport` | wallets status, total `elapsed_ms` |
| `runs_history.json` | slug, ok/fail paths (UI) |
| `measure_latency` | pre-run RPC/proxy only |

## 9.3 Модель данных

Новый модуль: `crates/minter-core/src/metrics.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunMetrics {
    pub run_id: String,              // slug_timestamp or uuid
    pub kind: String,                // "opensea" | "rawSniper" | "rawMint"
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunSpans {
    pub auth_ms: Option<u64>,
    pub nonce_ms: Option<u64>,
    pub balance_ms: Option<u64>,
    pub wait_ms: Option<u64>,
    pub prefetch_ms: Option<u64>,
    pub prep_sign_ms: Option<u64>,     // raw sniper
    pub first_send_ack_ms: Option<u64>, // offset from t0
    pub first_confirm_ms: Option<u64>,
    pub done_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSignalMetrics {
    pub kind: String,
    pub open_ts: Option<i64>,
    pub fire_lag_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletMetrics {
    pub address: String,
    pub proxy: Option<String>,
    pub status: String,
    pub auth_ms: Option<u64>,
    pub prefetch_ok: Option<bool>,
    pub send_attempts: u32,
    pub t_send_ack_ms: Option<u64>,
    pub t_confirm_ms: Option<u64>,
    pub tx_hash: Option<String>,
    pub gas_used: Option<u64>,
    pub error: Option<String>,
    pub error_class: Option<String>, // fatal | retryable | funds | rpc | cancel
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunMetricsSummary {
    pub total: usize,
    pub confirmed: usize,
    pub failed: usize,
    pub elapsed_ms: u64,
    pub fail_reasons: HashMap<String, usize>,
}
```

### Collector

```rust
pub struct MetricsCollector {
    inner: Mutex<RunMetrics>,
    t0: Instant,
    t0_unix_ms: i64,
}

impl MetricsCollector {
    pub fn mark_span(&self, name: &str, ms: u64) { /* … */ }
    pub fn mark_wallet_send(&self, addr: Address, attempts: u32, t_ms: u64) { /* … */ }
    pub fn set_open_signal(&self, …) { /* … */ }
    pub fn finish(self, results: &[MintResult]) -> RunMetrics { /* … */ }
}
```

Опционально: `MintEvent.metric: Option<MetricPoint>` — UI ignore, file log optional line `METRIC …`.

## 9.4 Точки инструментирования

### OpenSea `mint.rs`

| Mark | Когда |
|------|--------|
| t0 | start `run_opensea_mint` |
| auth_ms | after auth batch |
| nonce_ms | after nonce refresh |
| balance_ms | after balance gate |
| wait enter/exit | wait loop |
| open_signal | break wait |
| prefetch_ms | prefetch join |
| per-wallet send_ack | after sendRaw |
| per-wallet confirm | receipt OK |
| done | summary |

### Raw `raw_sniper.rs`

| Mark | Когда |
|------|--------|
| wait prep | until prep window |
| prep_sign_ms | pre-sign batch |
| open_signal | fire |
| first_send_ack | first successful send |
| receipts | post path |
| done | return |

## 9.5 Export

`export.rs`:

- писать `results/mint_{slug}_{ts}.metrics.json` рядом с json/csv;  
- или поле `metrics` внутри export (предпочтительнее **отдельный файл** — меньше breaking для consumers).

`MintRunSummary`:

```rust
pub metrics_path: Option<String>,
```

## 9.6 History UI

`runs_history.json` entry (extend, backward compatible):

```json
{
  "id": "…",
  "slug": "…",
  "chain": "…",
  "confirmed": 10,
  "failed": 2,
  "elapsedMs": 12345,
  "firstConfirmMs": 1800,
  "openLagMs": 12,
  "openSignal": "clock",
  "metricsPath": "results/….metrics.json",
  "exportJson": "…",
  "ts": "…"
}
```

UI History:

- list columns: slug, confirmed/failed, first confirm, open lag.  
- detail panel: spans table + fail_reasons + per-wallet times.

## 9.7 Файлы

| Файл | Действие |
|------|----------|
| `minter-core/src/metrics.rs` | **new** |
| `minter-core/src/lib.rs` | mod |
| `minter-core/src/mint.rs` | marks |
| `minter-core/src/raw_sniper.rs` | marks |
| `minter-core/src/export.rs` | write metrics json |
| `minter-core/src/progress.rs` | optional MetricPoint |
| `minter-core/src/api.rs` | wire summary paths |
| `ui/app.js` | history load/render |
| `ui/index.html` | detail panel |
| `ui/i18n.js` | labels |

## 9.8 Тесты

- collector mark order → spans filled.  
- `error_class` mapping from `classify_mint_error`.  
- serialize/deserialize roundtrip.  
- summary counts match wallet rows.

## 9.9 Оценка

- types + collector + tests: 1 day  
- mint/raw hooks: 1–1.5 day  
- export + history UI: 1–1.5 day  
- **Итого:** ~3–4.5 days  

**Риск:** low if marks are additive; watch lock contention (use short critical sections).

---

# 10. Hot-path tests + ABI completeness

## 10.1 Цель

1. Расширить ABI encoder (arrays → tuples).  
2. Unit/integration tests на policy + sniper/mint helpers.  
3. Safety net для PR #4/#7/#9.

## 10.2 ABI — текущее

`abi.rs` `build_calldata`:

- OK: static types, `string`, `bytes`.  
- FAIL: `T[]`, tuples `(…)`, fixed `T[n]` — `"arrays/tuples not implemented"`.

Парсер сигнатур **уже** считает depth для `(` `)` — tuples в parse ок, encode нет.

## 10.3 ABI roadmap

### Phase A — static element arrays (priority)

Types: `uint256[]`, `address[]`, `bytes32[]`, `bool[]`, …

Encoding:

- head: offset (32 bytes)  
- tail: length (32) + `n * 32` static elements  

UI param formats (accept all):

```
"[0xabc…,0xdef…]"
"0xabc…,0xdef…"
"1,2,3"
```

**Зачем:** merkle proofs `bytes32[]` в custom claims.

### Phase B — fixed arrays

`uint256[3]`, `address[2]` — head inline static words, no offset.

### Phase C — dynamic element arrays

`bytes[]`, `string[]` — tail offsets + per-element dynamic encoding.

### Phase D — tuples

`(address,uint256)`, `(address,uint256,bytes32[])`  

- static tuple: inline  
- dynamic tuple: offset + head/tail like multi-arg  

UI: `"(0xaddr,1)"` / JSON array.

### Implementation sketch

```rust
fn is_array_type(ty: &str) -> bool { /* ends with [] or [n] */ }
fn parse_array_type(ty: &str) -> Result<(String /*elem*/, Option<usize> /*fixed*/)>
fn encode_array(elem_ty: &str, values: &[String], fixed: Option<usize>) -> Result<Vec<u8>>
fn encode_tuple(inner_types: &[String], values: &[String]) -> Result<…>
```

Golden tests: compare against known hex from alloy/`cast abi-encode` samples committed as fixtures.

### Files

- `abi.rs` — encode paths + large `#[cfg(test)]`  
- raw mint UI help: «arrays: comma-separated or JSON array»

## 10.4 Unit tests (priority matrix)

| Module | Cases |
|--------|--------|
| `open_signal::should_fire` | all modes × clock × probes × early window |
| `funding::build_funding_plan` | math, empty, all ok, mixed deficit |
| `mint::classify_mint_error` | funds, InvalidProof, generic revert → retryable |
| `mint_ops::parse_at_time_unix` | already; add ms overflow edges |
| `raw_sniper::MintBayStatus::is_public_open` | phase type, pause, supply, window |
| `abi` arrays/tuples | golden vectors |
| `gas::calculate_fees` | auto/manual, multipliers |
| `metrics` collector | spans + summary |

## 10.5 Integration tests (mock RPC)

**Подход:** HTTP mock (e.g. `wiremock`) **или** internal trait:

```rust
// lighter: test-only RpcClient::new_with_handler(fn…)
```

Минимальные сценарии:

1. Raw sniper no `at_time`: pre-sign → send N wallets → results len.  
2. Cancel during wait → all cancelled.  
3. Hybrid: mock `eth_call` getMintStatus returns open before clock → fire.  
4. Balance gate: mock balances under need → wallet failed.  

Не в CI: live OpenSea SIWE / mainnet.

### Dev-deps

```toml
# minter-core/Cargo.toml
[dev-dependencies]
tokio = { version = "1", features = ["full", "test-util"] }
# wiremock = "0.6"  # if HTTP mock chosen
```

## 10.6 CI bar

```powershell
cargo test -p minter-core
cargo check -p minter-core -p minter-desktop
```

Optional later: coverage on `abi`, `funding`, `open_signal`, `mint_ops`.

## 10.7 PR split

| PR | Scope |
|----|--------|
| 10a | unit tests for existing pure fns + `classify_mint_error` export if needed |
| 10b | ABI `T[]` static + goldens |
| 10c | ABI tuples + fixed arrays |
| 10d | mock RPC integration (after #4) |

## 10.8 Оценка

| Кусок | Срок |
|-------|------|
| 10a unit pure | 0.5–1 day |
| 10b ABI arrays | 1–2 days |
| 10c ABI tuples | 1–2 days |
| 10d mock integration | 2–3 days |
| **Итого** | ~5–8 days (parallelizable with other PRs) |

**Риск:** low; ABI changes must not break existing signatures (regression tests on `mint(uint256)`, `string`, `bytes`).

---

## Сводка API / UI surface

### New Tauri commands

| Command | PR | In | Out |
|---------|----|----|-----|
| `plan_funding` | #7 | chain, addresses, price, qty, … | `FundingPlan` |
| `disperse_from_plan` | #7 | chain, from, plan, dryRun | rows |
| (optional) none for #4/#9 | — | fields on existing run/raw | events + files |

### New / extended settings fields

| Field | Where | Default |
|-------|--------|---------|
| `openMode` | task, raw sniper | `clock` |
| `earlyWindowSecs` | task, raw | `2` |
| `openConfirmPolls` | task, raw | `2` |
| gas buffer bps | funding UI | `1000` |

### New files on disk

```
results/mint_{slug}_{ts}.metrics.json   # #9
```

---

## Риски и mitigation

| Риск | Mitigation |
|------|------------|
| Hybrid false open early | `early_window` + `open_confirm_polls`; default mode clock |
| Api rate limit OpenSea | poll only near open; use existing proxy; soft-fail |
| Metrics lock on hot path | batch marks; no await under lock |
| Disperse overfund | show deficit table; user edits amount; dry-run first |
| ABI encode wrong | golden tests vs cast/alloy; ship arrays before tuples |
| Task JSON unknown fields | serde ignore unknown / default |

---

## Чеклист приёмки (E2E manual)

### #4

- [ ] Task mode `clock` — поведение идентично pre-change  
- [ ] Raw MintBay `hybrid` — fire when probe OPEN even if few seconds before clock (within window)  
- [ ] `event` without open — timeout/cancel clean  
- [ ] Log/metrics show `openSignal.kind`

### #7

- [ ] Plan shows correct need vs balances  
- [ ] Dry-run disperse from plan  
- [ ] Live top-up → balance gate passes on next mint dry-run  
- [ ] Fund eligible only filters WL

### #9

- [ ] After mint, `*.metrics.json` exists  
- [ ] History shows firstConfirmMs / openLag  
- [ ] Dry-run still writes metrics with dry_run true

### #10

- [ ] `cargo test -p minter-core`  
- [ ] Custom raw `mint(uint256[])` or proof array encodes and probe/sim works on known contract fixture

---

## Оценка суммарно

| PR | Содержание | Effort |
|----|------------|--------|
| PR0 | #10a unit + #10b ABI arrays | 2–3 d |
| PR1 | #7 funding v1 | 2.5–3.5 d |
| PR2 | #9 metrics | 3–4.5 d |
| PR3 | #4 hybrid open | 2.5–4 d |
| PR4 | #10c tuples + #10d mock | 3–5 d |
| **Всего** | | **~13–20 engineer-days** |

---

## Вне scope (этот документ)

- Multi-RPC send fan-out (#1 из brainstorm)  
- NTP clock sync (#2)  
- Multi-builder Flashbots alternatives (#3)  
- Adaptive gas ladder (#5)  
- Ready-score dashboard (#6)  
- Multi-task scheduler (#8)  
- UI rewrite React/Vue  
- Hardware wallets / cloud

Их можно добавить отдельными plan-файлами после закрытия PR0–PR4.

---

## Ссылки на код (якоря)

| Тема | Path |
|------|------|
| OpenSea wait loop | `crates/minter-core/src/mint.rs` |
| Error classify | `crates/minter-core/src/mint.rs` `classify_mint_error` |
| Raw sniper | `crates/minter-core/src/raw_sniper.rs` |
| MintBay open | `is_public_open`, `fetch_mintbay_status` |
| Disperse | `crates/minter-core/src/disperse.rs` |
| Session API | `crates/minter-core/src/api.rs` |
| Export | `crates/minter-core/src/export.rs` |
| Events | `crates/minter-core/src/progress.rs` |
| ABI | `crates/minter-core/src/abi.rs` |
| Desktop commands | `crates/minter-desktop/src-tauri/src/lib.rs` |
| UI | `crates/minter-desktop/ui/app.js` |

---

*Документ: план реализации. Не спецификация on-chain протоколов OpenSea/MintBay — при смене внешних API detector’ы править точечно.*
