# MINTER — план смягчения оставшихся рисков

План по findings из `docs/CODE_AUDIT.md` §6 (не блокеры).  
**Не ломаем** default sniper/mint: wall-clock open, pre-sign race, success = confirm, burner-only product.

| # | Уровень | Тема | Тип работ | Приоритет внедрения |
|---|---------|------|-----------|---------------------|
| M6 | medium | Не zip’ить live vault/config | packaging / docs / script | **P0** (быстро, high safety) |
| M2 | medium | OpenSea 429 без proxies | product guard + UX | **P1** |
| M4 | medium | Tasks Start = LIVE без CONFIRM | product safety UX | **P1** |
| M1 | medium | L2 sniper без fee re-sign | opt-in feature | **P2** |
| M3 | medium | keys/password in RAM | security hardening | **P2** |
| L1 | low | Clippy noise | polish | **P3** |
| L5–L6 | low | mock-RPC / hybrid open | features (IMPROVEMENT) | **P3+** (отдельные треки) |

**Связанные docs:** `CODE_AUDIT.md`, `BUGFIX_PLAN.md` (done), `IMPROVEMENT_PLAN.md` (#4, #10), `ARCHITECTURE.md`.

---

## Принципы

1. **Opt-in** для поведения, которое меняет latency (M1 L2 re-sign) или UX (M4 confirm).  
2. Default sniper остаётся «быстрым»; safety — additive.  
3. После каждого merge: `cargo test -p minter-core` + `package-public.ps1` → Public.  
4. `target\` disposable; runtime = Public only.

---

# M6 — Не zip’ить live vault/config из `Public\`

### Проблема
В dev-дереве `Public\` лежат live `keys.vault`, `config.json`, `auth_cache.bin`, results.  
Случайный zip всей папки = утечка секретов.

### Цель
Сделать «безопасный package» однозначным и машиночитаемым.

### План работ

| Шаг | Действие | Файлы |
|-----|----------|--------|
| M6.1 | Расширить `package-public.ps1`: опциональный **clean zip** только allowlist | `scripts/package-public.ps1` |
| M6.2 | Allowlist: `minter-desktop.exe`, `config.example.json`, `proxies.example.txt`, `ИНСТРУКЦИЯ.md`, `README.txt` | script |
| M6.3 | Explicit denylist log: never pack `keys.vault`, `config.json`, `*.bin`, `tasks.json`, `wallet_meta.json`, `runs_history.json`, `proxies.txt`, `results/`, `logs/` | script |
| M6.4 | `Public/.ship-ignore` или `Public/SHIP_MANIFEST.txt` — список того, что уходит в zip | `Public/` |
| M6.5 | Краткая секция «Как сделать zip для друзей» в ИНСТРУКЦИЯ + README | docs |

### Рекомендуемый UX скрипта

```powershell
# как сейчас — только exe
.\scripts\package-public.ps1

# + собрать Public\minter-desktop-0.1.0-windows.zip (safe)
.\scripts\package-public.ps1 -MakeZip
```

### DoD
- [x] `-MakeZip` создаёт архив **без** vault/config/results  
- [x] В логе script перечисляет excluded secrets  
- [x] Docs описывают один правильный способ шаринга  

### Effort
~0.5–1 день. **Стартовать первым.**

---

# M2 — OpenSea 429 без proxies

### Проблема
Multi-wallet SIWE / GQL / eligibility с одного IP → 429, failed auth, «all wallets failed».

### Цель
Снизить 429 **до** fire; ясный UX «добавь proxies».

### План (слои)

#### A. Soft gates (UX, без смены hot path) — recommended v1

| Шаг | Действие |
|-----|----------|
| M2.1 | Pre-flight checklist: если `wallets ≥ N` (напр. 4) и `proxies == 0` → **warning banner** + confirm «Continue anyway?» |
| M2.2 | Settings/Tasks: tip link «OpenSea rate-limits direct IP» (i18n EN/RU) |
| M2.3 | Лог auth: при 429 — classify + «reduce AUTH_CONCURRENCY / add proxies» |

#### B. Adaptive concurrency (core)

| Шаг | Действие | Файлы |
|-----|----------|--------|
| M2.4 | При 429 в SIWE/GQL: backoff + **временно** `concurrency = 1` на N секунд | `opensea.rs`, `mint.rs` |
| M2.5 | Env/settings: `AUTH_CONCURRENCY` уже есть — UI expose (optional) | Settings UI |
| M2.6 | Eligibility/warm_auth: единый policy с mint (не выше 2 без proxy) | `api.rs` |

#### C. Hard gate (optional, product)

| Шаг | Действие |
|-----|----------|
| M2.7 | Setting `require_proxies_for_n_wallets` (default off): block Start если wallets ≥ N и proxies = 0 |

### Default recommendation
- **M2.1–M2.3 + M2.4** (soft UX + adaptive backoff).  
- Hard block **opt-in** (M2.7), не default.

### DoD
- [x] ≥4 wallets, 0 proxies → явный warning перед Start  
- [x] 429 в логе с actionable message  
- [x] Adaptive: после 429 auth не долбит 6 parallel на том же IP  
- [x] i18n EN/RU  

### Effort
v1 soft: 1–2 дня. Adaptive: +1–2 дня.

---

# M4 — Tasks Start = LIVE без CONFIRM

### Проблема
Operator error: Start тратит реальный ETH; глобальный Dry Run chip **не** влияет на Tasks Start.  
`confirm` в API **игнорируется** (by design).

### Цель
Сохранить sniper speed, но снизить accidental live.

### Варианты (выбрать один product path)

| Option | Поведение | Latency | UX risk |
|--------|-----------|---------|---------|
| **A (Recommended)** | Modal **«LIVE mint — type LIVE»** (or checkbox «I understand») once per Start when `!dry_run` | +1 click | low |
| **B** | Tasks Start respects global Dry Run chip unless task overrides `forceLive` | 0 | confuses power users |
| **C** | Settings `require_live_confirm: bool` default **true** for new installs; power users disable | configurable | best balance |
| **D** | Keep no confirm; only stronger LIVE badge + red Start | 0 | weak |

### План (рекомендация: **C + A**)

| Шаг | Действие | Файлы |
|-----|----------|--------|
| M4.1 | Setting `require_live_confirm` (default **true** in `Settings::default`) | `settings.rs`, config.json |
| M4.2 | UI: if live && require → modal: short summary (slug, wallets, dry/live) + type `LIVE` or confirm button | `app.js`, `i18n.js` |
| M4.3 | Tauri: still ignore typed string in core, but **desktop** blocks invoke until UI confirm | `lib.rs` only if needed |
| M4.4 | Docs: ИНСТРУКЦИЯ — «Start LIVE; optional confirm in Settings» | Public docs |
| M4.5 | Power users: Settings → uncheck require confirm (sniper mode) | UI |

### Не делать
- Возвращать hard `CONFIRM` в core mint path (ломает automation / API stability).  
- Молча менять Tasks Start на global dry_run без флага.

### DoD
- [x] Fresh install: Start LIVE requires explicit UI confirm  
- [x] Sniper preset / setting can disable confirm  
- [x] Dry-run paths never ask LIVE confirm  
- [x] Docs EN/RU  

### Effort
1–2 дня.

---

# M1 — L2 sniper без fee re-sign

### Проблема
Raw sniper pre-sign на T−5s; fee re-sign at fire **только** `chain_id == 1`.  
На L2 base/priority иногда растут → underpriced / slow inclusion (редко).

### Цель
Opt-in L2 fee refresh **без** дефолтного +latency на Base snipes.

### План

| Шаг | Действие | Файлы |
|-----|----------|--------|
| M1.1 | Config flag `refresh_fees_at_fire: enum { MainnetOnly, Always, Never }` default **MainnetOnly** | `raw_sniper.rs`, Settings / raw form |
| M1.2 | `Always`: same path as L1 (fee_history → re-sign if higher) | `raw_sniper.rs` |
| M1.3 | UI Raw Sniper: checkbox «Refresh fees at fire (adds latency)» | `app.js`, i18n |
| M1.4 | Log: `fee refresh skipped (L2 default)` vs `re-signed N` | reporter |
| M1.5 | Unit: pure helper «should_refresh_fees(chain_id, mode)» | tests |

### DoD
- [x] Default Base: **no** extra fee_history at T0  
- [x] Opt-in Always works on Base  
- [x] Never disables even mainnet refresh  
- [x] Docs note in ИНСТРУКЦИЯ / Raw help  

### Effort
0.5–1 день.

---

# M3 — keys/password in RAM while unlocked

### Проблема
`Session` holds `password: Zeroizing` + `signers: Vec<Signer>` until lock/exit.  
Clone session for async mint keeps material in memory.

### Цель
Снизить window exposure **без** ломания multi-command UX.

### План (фазы)

#### Phase A — idle auto-lock (recommended)

| Шаг | Действие |
|-----|----------|
| M3.1 | Setting `idle_lock_minutes` (default 30, 0 = off) |
| M3.2 | Desktop: track last user action; timer → `session.lock()` (clear signers + password) |
| M3.3 | If mint_running: **don't** lock until mint done (or cancel) |
| M3.4 | UI: toast «Vault locked due to idle» → re-unlock |

#### Phase B — reduce clones (deeper)

| Шаг | Действие |
|-----|----------|
| M3.5 | Avoid full `Session` clone for mint: pass `Arc<signers>` + password only when needed |
| M3.6 | Zeroize password after auth_cache flush if not needed mid-mint (auth uses tokens) — careful: re-auth needs password only for cache encrypt, not SIWE |
| M3.7 | Document threat model: local malware still wins if process unlocked |

#### Phase C — optional (OS)

| Шаг | Действие |
|-----|----------|
| M3.8 | Windows: optional SecureString / DPAPI for cache key (out of scope v1) |

### DoD (Phase A)
- [x] Idle lock works; mint not interrupted mid-run  
- [x] Unlock restores signers  
- [x] Setting 0 disables  
- [x] Docs  

### Effort
Phase A: 1–2 дня. Phase B: 2–3 дня.

---

# L1 — Clippy noise

### Проблема
~70 clippy warnings (style): `collapsible_if`, `too_many_arguments`, `useless_format`, etc.  
Не runtime.

### План

| Шаг | Действие |
|-----|----------|
| L1.1 | `cargo clippy -p minter-core -- -W clippy::all` baseline list |
| L1.2 | Auto-fix safe: collapsible_if, useless_format, next_back, redundant_pattern_matching |
| L1.3 | `#[allow(clippy::too_many_arguments)]` on intentional mint helpers **or** introduce params structs (larger) |
| L1.4 | CI optional: `clippy -D warnings` later when clean |

### DoD
- [x] `cargo clippy -p minter-core -- -D warnings` green **or** allowlist documented  
- [x] No behaviour change  

### Effort
0.5–1 день (allows) / 2+ days (real refactors).

---

# L5–L6 — mock-RPC / hybrid open

### Связь
Уже в **`docs/IMPROVEMENT_PLAN.md`**:
- **#10** Hot-path tests + mock RPC (L5)  
- **#4** Event-driven / hybrid open (L6)  

### Здесь — только sequencing, не дублировать полный design

| ID | Трек | Когда | Зависимости |
|----|------|-------|-------------|
| L5 | PR0 из IMPROVEMENT: unit + mock RPC skeleton | После M6/M2/M4 | tests safety net |
| L6 | Hybrid open (clock ∥ API ∥ on-chain) | После metrics (#9) ideally | open_signal module |

### L5 mini-plan (extract)

1. Trait/`MockRpc` for `eth_blockNumber`, `estimateGas`, `sendRaw`, receipts.  
2. Integration test: dry OpenSea path without network (fixture GQL optional).  
3. ABI tuples completeness (IMPROVEMENT #10 full).

### L6 mini-plan (extract)

1. `OpenMode { Clock, Hybrid, Event }` default **Clock**.  
2. `should_fire` pure + unit tests.  
3. Wire mint wait loop + raw sniper optional.  
4. UI opt-in.

### DoD
- См. IMPROVEMENT_PLAN Definition of Done.  
- Default behaviour **Clock** unchanged.

### Effort
L5 partial: 2–4 дня. L6 v1: 3–5 дней.

---

## Порядок внедрения (DAG)

```
M6 packaging zip safety          ──┐
                                   ├──► quick safety release
M2.1–2.3 UX 429 warnings         ──┤
M4 LIVE confirm (settings+modal) ──┘
         │
         ▼
M2.4 adaptive 429 backoff
         │
         ▼
M1 opt-in L2 fee refresh
M3.A idle auto-lock
         │
         ▼
L1 clippy polish
         │
         ▼
L5 mock RPC (IMPROVEMENT #10)
L6 hybrid open (IMPROVEMENT #4)  ← can parallel after L5 tests
```

### Suggested milestones

| Milestone | Items | Outcome |
|-----------|--------|---------|
| **R1 Safety pack** | M6 + M2 soft + M4 | Safer share + fewer accidents |
| **R2 Resilience** | M2 adaptive + M1 + M3.A | Better 429 / fees / idle lock |
| **R3 Polish & future** | L1 + L5 + L6 | Clean lint + feature roadmap |

---

## Что **не** в этом плане

| Item | Почему |
|------|--------|
| M5 Flashbots inclusion | Expected network behaviour; messaging only if needed |
| L2/L3/L4 audit notes | Cosmetic / intentional |
| Breaking Tasks Start always dry | Product reject |
| Default L2 fee refresh Always | Adds sniper latency |

---

## Definition of Done (весь risk plan)

- [x] M6: safe zip script  
- [x] M2: warning + adaptive 429  
- [x] M4: opt-out LIVE confirm  
- [x] M1: fee refresh mode enum  
- [x] M3: idle lock  
- [x] L1: clippy clean or allowlisted  
- [x] L5/L6: tracked in IMPROVEMENT, not blocked  
- [x] `cargo test -p minter-core` green  
- [x] `package-public.ps1` → Public  
- [x] ИНСТРУКЦИЯ / README updated for user-facing changes  

---

## Effort estimate (rough)

| Milestone | Calendar |
|-----------|----------|
| R1 | 2–4 days |
| R2 | 3–5 days |
| R3 (L1 only) | 0.5–1 day |
| R3 (L5+L6) | 1–2 weeks |

---

## История

| Дата | Событие |
|------|---------|
| 2026-07-16 | Plan created from CODE_AUDIT remaining risks |
| 2026-07-16 | R1+R2+L1 implemented (M6,M2,M4,M1,M3.A,L1); L5/L6 still tracked only |
