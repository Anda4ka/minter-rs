# MINTER — аудит кода (2026-07-16)

Аудит после полного цикла runtime-фиксов **P0 + P1 + P2** (`docs/BUGFIX_PLAN.md`).  
Продукт для запуска: **только** `Public\minter-desktop.exe`.

---

## 1. Область

| Область | Охват |
|---------|--------|
| `crates/minter-core` | mint, raw sniper, RPC, vault, gas, auth, OpenSea, API |
| `crates/minter-desktop` | Tauri commands, cancel/busy, events |
| UI | `ui/app.js` (spot-check escapeHtml / invoke) |
| Docs / packaging | Public-only workflow |

**Не в scope:** e2e против live OpenSea/RPC, security pen-test, legal.

---

## 2. Сводка

| Категория | Оценка |
|-----------|--------|
| Компиляция / unit tests | **OK** — `cargo test -p minter-core` **99 passed** |
| Runtime-баги (BUGFIX_PLAN) | **Закрыты** P0–P2 (кроме clippy polish) |
| Hot-path mint/sniper | Зрелый; wall-clock open + confirm = success |
| Безопасность ключей | Vault AES-GCM + PBKDF2 600k; atomic write |
| Ops / packaging | Public-only; `target\` disposable |
| Feature gaps | Hybrid open, funding, metrics — planned |

**Вердикт:** кодовая база **готова к burner-use** при корректном RPC/прокси. Оставшиеся пункты — улучшения и manual smoke, не блокеры сборки.

---

## 3. Архитектура (кратко)

```
Public\minter-desktop.exe  (Tauri 2 + vanilla UI)
        │  invoke / events
        ▼
minter-desktop (src-tauri)  — Session lock, mint_running, cancel
        │
        ▼
minter-core
  mint.rs / raw_sniper.rs   — hot path
  opensea.rs / auth_cache   — SIWE + cache flush
  rpc.rs / gas.rs / sign    — chain I/O
  vault.rs / settings       — secrets + config.json
```

**Default open:** wall-clock (`stage.start_time` / `at_time`).  
**Success:** `WalletStatus::Confirmed` (или DryRunOk). **SENT ≠ success.**

---

## 4. Закрытые runtime-баги (регрессии)

| ID | Тема | Статус | Где |
|----|------|--------|-----|
| P0.1 | Proxy re-auth после auth-cache scramble | fixed | `WalletAuth.proxy_url` |
| P0.2 | OpenSea gas без L2 floor | fixed | `resolve_mint_gas_limit` |
| P0.3 | Silent empty calldata | fixed | `parse_tx_calldata_hex` |
| P1.1 | fire_lag только секунды | fixed | `fire_lag_ms_from_clock` |
| P1.2 | Auth cache KDF на каждый wallet | fixed | `save` + `flush` |
| P1.3 | Raw sniper stale fees L1 | fixed | fee refresh + re-sign @ fire |
| P1.4 | RBF poll только last hash | fixed | `wait_for_any_receipt` |
| P2.1 | Bad `at_time` → silent phase start | fixed | `bail!` via `parse_at_time_unix` |
| P2.2 | Receipt wait confusing warn | fixed | `started.elapsed()` |
| P2.3 | f64 fee bumps | fixed | `bump_fee_bps` / `gwei_str_to_wei` |

---

## 5. Сильные стороны

1. **Vault** — AES-256-GCM, 600k PBKDF2, atomic write (+ Windows bak), `Zeroizing` password.
2. **Amount path** — `eth_to_wei` без f64 (0.08 ETH exact); gwei→wei через decimal units.
3. **Success semantics** — confirm only; UI beep on first on-chain confirm.
4. **Wall-clock open** — осознанный anti-lag vs `block.timestamp`.
5. **RPC** — multi-URL race send, 5s call timeout, receipt multi-hash (RBF).
6. **Cancel** — `AtomicBool` между attempts / wait loops.
7. **Flashbots** — mainnet-only gate, callBundle dry, coordinator after workers.
8. **Tests** — 99 unit tests (gas, amount, vault, auth flush, mint helpers, mintbay, receipt).
9. **Auth cache** — encrypted; batch flush (no N×KDF).
10. **Proxy binding** — sticky per wallet from auth time.

---

## 6. Оставшиеся риски / findings

### 6.1 Средние (не блокеры) — post RISK_MITIGATION

| # | Finding | Status (2026-07-16) |
|---|---------|---------------------|
| M1 | L2 sniper fee re-sign | **Mitigated** — `FeeRefreshMode` default MainnetOnly; Always opt-in |
| M2 | OpenSea 429 multi-wallet | **Mitigated** — UI warn ≥4/0 proxies; 429 serial auth + actionable log |
| M3 | keys in RAM while unlocked | **Mitigated (A)** — idle auto-lock default 30m; skip mid-mint |
| M4 | LIVE Start without confirm | **Mitigated** — `require_live_confirm` default on; type LIVE modal |
| M5 | Flashbots inclusion | Unchanged (expected network) |
| M6 | Zip live vault from Public | **Mitigated** — `-MakeZip` allowlist only |

### 6.2 Низкие

| # | Finding | Status |
|---|---------|--------|
| L1 | Clippy noise | **Mitigated** — crate allowlist in `lib.rs` (documented) |
| L2 | edition mismatch | Deferred |
| L3 | Dry-run f64 display | OK display-only |
| L4 | gas mult floor 1.15 | Intentional |
| L5 | mock-RPC integration | **Still planned** IMPROVEMENT #10 |
| L6 | Hybrid open | **Still planned** IMPROVEMENT #4 |

### 6.3 Security notes (burner product)

| Topic | Status |
|-------|--------|
| Keys leave machine? | No (local vault / RPC / OpenSea SIWE only) |
| Telemetry | None |
| Auth tokens on disk | Encrypted `auth_cache.bin` (password-derived) |
| Confirm gate on Tasks Start | **Default on** — type `LIVE` when `require_live_confirm` (disable via Settings / Sniper preset) |
| XSS in UI | Mostly `escapeHtml`; keep auditing new templates |

---

## 7. Hot-path checklist (operator)

| Step | OpenSea mint | Raw sniper |
|------|--------------|------------|
| Auth | SIWE + cache flush once | N/A |
| Wait open | Wall clock + lag ms | Pre-sign T−5s → clock fire |
| Gas | estimate + L2 floor / fixed clamp | Hard limit default 650k |
| Fees | fee_history @ fire | L1 re-sign if fees rose |
| Send | race multi-RPC | blast pre-signed |
| Confirm | multi-hash RBF poll | receipt 90s |

---

## 8. Test / build gate

```powershell
cargo test -p minter-core
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
# → Public\minter-desktop.exe
# optional: remove-item target -Recurse -Force
```

| Gate | Result (2026-07-16) |
|------|---------------------|
| Unit tests | 99 passed |
| Package Public | required after any core/UI change |
| Manual smoke | recommended: dry raw sniper, WL check, one dry path |

---

## 9. Manual smoke (recommended)

1. Unlock vault → list wallets.  
2. RPCs Probe OK.  
3. Warm auth / WL check multi-wallet with proxies — logs show expected proxy.  
4. Base network: dry raw mint — gas_limit ≥ 150k in logs if estimate path.  
5. Task with garbage `at_time` → clear error (not silent wait).  
6. Live only with burner + known phase.

---

## 10. Связанные документы

| Doc | Role |
|-----|------|
| `README.md` | Product overview + build |
| `ЗАПУСК.md` | Quick start RU |
| `Public/ИНСТРУКЦИЯ.md` | End-user guide |
| `Public/README.txt` | Zip contents |
| `docs/BUGFIX_PLAN.md` | P0–P2 status |
| `docs/IMPROVEMENT_PLAN.md` | Future features |
| `docs/ARCHITECTURE.md` | Module map |
| `docs/CODE_AUDIT.md` | This file |
| `docs/RISK_MITIGATION_PLAN.md` | Plan for remaining M*/L* risks |

---

## 11. История аудита

| Дата | Событие |
|------|---------|
| 2026-07-16 | Full audit post P0–P2; 99 tests; Public-only delivery |
| 2026-07-16 | Post RISK_MITIGATION R1+R2+L1: M1–M4,M6,L1 shipped; L5/L6 deferred; 104 tests |
