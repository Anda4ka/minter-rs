# MINTER — план фикса runtime-багов

> **Статус: P0 + P1 + P2 DONE** (2026-07-16). Clippy polish deferred.  
> Аудит после фиксов: `docs/CODE_AUDIT.md`. Архитектура: `docs/ARCHITECTURE.md`.

Документ фиксирует **план и историю** runtime-багов, плюс workflow доставки **только** в `Public/`.

**Стек:** `minter-core` + `minter-desktop` (Tauri 2).  
**Runtime-продукт:** только `Public\minter-desktop.exe` (+ config / vault / proxies рядом).  
**Default behaviour не ломаем:** wall-clock open, pre-sign race, success = on-chain confirm.

---

## Public vs `target/`

| Путь | Для запуска | Для сборки |
|------|-------------|------------|
| `Public\` | **Да** — единственный runtime | Нет |
| `target\` | **Нет** | Временный кэш Cargo на время build |

### Как устроено

1. `cargo build -p minter-desktop --release` пишет exe в `target\release\minter-desktop.exe`.
2. `scripts\package-public.ps1` **копирует** его в `Public\minter-desktop.exe`.
3. Запуск всегда: `.\Public\minter-desktop.exe`.

### Политика репозитория

- **`target\` не нужен для работы** — удаляется после успешного package (или до следующего билда).
- После фикса кода — **один** шаг доставки:

```powershell
# из корня репо
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
.\Public\minter-desktop.exe
```

- Не хранить / не коммитить `target\`.
- Не затирать в `Public\` live-данные: `keys.vault`, `config.json`, `tasks.json`, `proxies.txt` (скрипт копирует только exe).

---

## Принципы фиксов

1. Default sniper/mint behaviour без сюрпризов.
2. После каждого P0/P1 блока: `cargo test -p minter-core` + `cargo check -p minter-desktop`.
3. Unit-тесты на вынесенные helpers.
4. Финальная доставка: `package-public.ps1` → `Public\minter-desktop.exe` → (опционально) удалить `target\`.

---

## Фаза 0 — подготовка

| # | Действие | Статус |
|---|----------|--------|
| 0.1 | Baseline: `cargo test -p minter-core` | |
| 0.2 | Ветка `fix/runtime-bugs` (опционально) | |
| 0.3 | Не трогать live vault/config в `Public\` | |
| 0.4 | `target\` удалён (build cache; воссоздаётся cargo) | done (policy) |

---

## Фаза 1 — P0 (критичные runtime)

### 1.1 Proxy re-auth: порядок `wallets` ≠ `signers` — **DONE**

| | |
|--|--|
| **Файлы** | `crates/minter-core/src/mint.rs`, `proxy.rs` (`short_proxy` pub) |
| **Фикс** | `WalletAuth.proxy_url` задаётся в auth loop по индексу `signers`; re-auth / logs только из него |
| **DoD** | Re-auth = тот же proxy, что SIWE |

### 1.2 Gas limit OpenSea mint без L2 floor — **DONE**

| | |
|--|--|
| **Файлы** | `mint.rs` (`resolve_mint_gas_limit` → `apply_gas_limit` / L2 clamp) |
| **Фикс** | Estimate path: `apply_gas_limit`. Fixed/SKIP_PREFLIGHT: clamp up на elevated chains + log |
| **Тест** | `mint_gas_estimate_applies_l2_floor`, `mint_gas_fixed_clamps_l2_floor` |
| **DoD** | Auto-gas на Base/Arb/OP/… ≥ L2 floor |

### 1.3 Empty / invalid calldata — fail-fast — **DONE**

| | |
|--|--|
| **Файлы** | `mint.rs` (`parse_tx_calldata_hex`) |
| **Фикс** | GQL: err без empty send. Local build: invalid → fallback GQL |
| **Тест** | `parse_calldata_*` unit tests |
| **DoD** | Нет send с `data=[]` из-за parse fail |

**Gate фазы 1:** `cargo test -p minter-core` green — **92 passed** (2026-07-16).

---

## Фаза 2 — P1 (точность / perf / sniper) — **DONE**

### 2.1 `fire_lag_ms` — реальные миллисекунды — **DONE**

| | |
|--|--|
| **Файл** | `mint.rs` (`fire_lag_ms_from_clock`) |
| **Фикс** | `timestamp_millis() − start_ts*1000`, saturating |
| **Тест** | `fire_lag_uses_millis_not_seconds` |

### 2.2 Auth cache: не PBKDF2 600k на каждый wallet — **DONE**

| | |
|--|--|
| **Файлы** | `auth_cache.rs`, `mint.rs`, `api.rs` (warm_auth / eligibility) |
| **Фикс** | `save()` memory-only + dirty; `flush()` один раз после batch |
| **Тест** | `save_is_memory_only_until_flush`, `multi_save_one_flush_roundtrip` |

### 2.3 Raw sniper: stale fees на T0 (mainnet) — **DONE**

| | |
|--|--|
| **Файл** | `raw_sniper.rs` |
| **Фикс** | `chain_id == 1`: fee_history at fire → re-sign if fees rose; L2 skip |
| **DoD** | L1 fees свежие к blast; L2 без extra RPC |

### 2.4 RBF: receipt по любому candidate hash — **DONE**

| | |
|--|--|
| **Файлы** | `rpc.rs` (`wait_for_any_receipt`), `mint.rs` RBF loop |
| **Фикс** | Candidate list original+RBF; first receipt wins |
| **DoD** | Confirm если mined **любая** из replacement/original |

**Gate фазы 2:** `cargo test -p minter-core` — **95 passed** (2026-07-16).

---

## Фаза 3 — P2 (UX / cleanup) — **DONE**

### 3.1 Invalid `at_time` → hard error в core — **DONE**

| | |
|--|--|
| **Файл** | `mint.rs` (`parse_at_time_unix`) |
| **Фикс** | Invalid `at_time` → `bail!`, без fallback на phase start |
| **Тест** | `parse_at_time_invalid_is_err_not_silent` |

### 3.2 `wait_for_receipt` — читаемость / safety — **DONE**

| | |
|--|--|
| **Файл** | `rpc.rs` (`wait_for_any_receipt`) |
| **Фикс** | warn via `started.elapsed()`; safe short hash; backoff cap 1s |

### 3.3 gwei / RBF bump без f64 — **DONE**

| | |
|--|--|
| **Файлы** | `gas.rs` (`bump_fee_bps`, `gwei_str_to_wei`), `types.rs`, `mint.rs`, `api.rs` |
| **Фикс** | gwei string → wei (9 decimals); RBF ×1.30 / underpriced ×1.15 via bps |

### 3.4 Clippy noise

- Не в scope этого fix-pass (отдельный polish-коммит при желании)

**Gate фазы 3:** `cargo test -p minter-core` — **99 passed** (2026-07-16).

---

## Фаза 4 — доставка в Public

| # | Действие |
|---|----------|
| 4.1 | `powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1` |
| 4.2 | Проверить время изменения `Public\minter-desktop.exe` |
| 4.3 | Smoke: unlock → dry-run task → raw sniper dry-run |
| 4.4 | Удалить `target\` (освободить диск; exe в Public уже независим) |
| 4.5 | Live smoke только при готовности к gas/mint spend |

### Скрипт package (напоминание)

`scripts/package-public.ps1`:

- `cargo build -p minter-desktop --release`
- copy → `Public\minter-desktop.exe`
- не трогает vault/config

---

## Порядок внедрения (DAG)

```
P0-1 proxy WalletAuth ────────┐
P0-2 apply_gas_limit mint ────┼──► cargo test ──► P1 fire_lag + auth flush
P0-3 calldata fail-fast ──────┘                      │
                                                     ▼
                              P1 raw L1 fees + RBF multi-hash
                                                     │
                                                     ▼
                              P2 at_time bail + receipt cleanup
                                                     │
                                                     ▼
                              package-public.ps1
                                                     │
                                                     ▼
                              Public\minter-desktop.exe
                                                     │
                                                     ▼
                              remove target\  (optional, recommended)
```

Параллельно безопасны: **1.1 / 1.2 / 1.3**.

---

## Чеклист DoD (весь план)

- [x] `cargo test -p minter-core` — all green (99)
- [x] P0+P1+P2 packaged: `Public\minter-desktop.exe` via `package-public.ps1`
- [x] `target\` удалён после package (политика)
- [x] fire lag: sub-second мс (unit)
- [x] auth_cache: memory save + one flush (unit)
- [x] RBF multi-hash poll API (`wait_for_any_receipt`)
- [x] L1 sniper fee refresh at fire
- [x] Invalid at_time: hard error (unit + core path)
- [x] integer gwei / fee bps bumps
- [ ] Proxy: partial auth cache → re-auth (manual smoke)
- [ ] Base/Arb dry-run: gas ≥ L2 floor (manual smoke)
- [ ] Clippy polish (optional)

---

## Вне scope (не баги этого плана)

| Тема | Где |
|------|-----|
| Hybrid open / funding / metrics | `docs/IMPROVEMENT_PLAN.md` |
| Mock RPC e2e / ABI tuples full | IMPROVEMENT_PLAN #10 |
| Смена default open mode на event | feature, ломает default |
| Другие бинарники кроме Public | нет; только `Public\minter-desktop.exe` |

---

## Сводка приоритетов

| ID | Severity | Кратко |
|----|----------|--------|
| 1.1 | P0 | Proxy re-auth wrong index | **done** |
| 1.2 | P0 | Mint gas без L2 floor | **done** |
| 1.3 | P0 | Silent empty calldata | **done** |
| 2.1 | P1 | fire_lag_ms seconds×1000 | **done** |
| 2.2 | P1 | Auth cache KDF per save | **done** |
| 2.3 | P1 | Sniper stale fees L1 | **done** |
| 2.4 | P1 | RBF multi-hash poll | **done** |
| 3.1 | P2 | at_time hard error | **done** |
| 3.2 | P2 | wait_for_receipt cleanup | **done** |
| 3.3 | P2 | integer fee bumps | **done** |
| 3.4 | P2 | clippy noise | deferred |

---

## История

| Дата | Событие |
|------|---------|
| 2026-07-16 | План создан по full code review; runtime = Public only; `target\` = disposable build cache |
| 2026-07-16 | P0.1–P0.3 implemented; tests 92; package → `Public\minter-desktop.exe` |
| 2026-07-16 | P1.1–P1.4 implemented; tests 95; package → `Public\minter-desktop.exe` |
| 2026-07-16 | P2.1–P2.3 implemented; tests 99; package → `Public\minter-desktop.exe`; clippy deferred |
