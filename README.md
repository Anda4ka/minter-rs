# MINTER

Local Windows desktop app for **OpenSea drop mints** and **raw contract sniping**.  
Multi-wallet, encrypted vault, proxies, task queue, results export.

**Stack:** Rust (`minter-core`) + Tauri 2 (`minter-desktop`).  
**No cloud, no telemetry.** Keys stay on this machine.

> **Burner wallets only.** Never import long-term funds.

---

## Runtime product

The only app you run day-to-day:

```text
Public\minter-desktop.exe
```

Config, vault, tasks, proxies, results and logs live **next to the exe** in `Public\` (or your unzip folder).

| Path | Role |
|------|------|
| `Public\` | **Shipped / daily runtime** |
| `target\` | Cargo build cache only — **not required to run** |
| `crates\` | Source — rebuild when you change code |

After code changes:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
.\Public\minter-desktop.exe
# optional: Remove-Item -Recurse -Force target
```

---

## Features

- **Encrypted vault** — AES-GCM, password-protected, atomic writes  
- **Wallets** — import, A/B/C groups, **balance by network**, per-wallet proxy  
- **OpenSea drops** — slug/URL, phase picker, WL / eligibility export  
- **Tasks** — save/edit/queue, Start/Stop (Start = **LIVE**; type `LIVE` confirm by default)  
- **Mission Control** — live HUD overlay on OpenSea Start (phase, stats, wallets, log)  
- **Sniper** — wall-clock phase open → sim → send → **on-chain confirm**  
- **Raw Mint** — multi-wallet pre-sign race; Discover (proxy EIP-1167/1967 + 4byte); Simple `mint(uint256)`  
- **RPC** — private Alchemy multi-chain (own API key only), **Ping networks** (+ via proxy), latency  
- **Proxies** — HTTP/SOCKS, health checks, sticky wallet mapping  
- **Results** — JSON/CSV, run history, explorer links, full mint logs  
- **UI** — dark-only, EN/RU, phase banner, first-confirm badge + optional beep  

### Mint flow

```text
Start → auth / prep → wait phase open (wall clock)
     → estimate (sim) → if OK: sign + send
     → wait receipt (RBF multi-hash) → CONFIRMED = success
```

`SENT` = broadcast only. **Success is only after block confirmation.**

---

## Stack

| Crate | Role |
|-------|------|
| `minter-core` | Engine: mint, vault, OpenSea, RPC, gas, sniper, export |
| `minter-desktop` | Tauri GUI + commands |

See `docs/ARCHITECTURE.md` for module map.

---

## Build & test

```powershell
# Release into Public\
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1

# Unit tests
cargo test -p minter-core

# Check only
cargo check -p minter-core -p minter-desktop
```

---

## Documentation

| Doc | Audience |
|-----|----------|
| [`ЗАПУСК.md`](ЗАПУСК.md) | Quick start (RU) |
| [`Public/ИНСТРУКЦИЯ.md`](Public/ИНСТРУКЦИЯ.md) | Full end-user guide (RU) |
| [`Public/README.txt`](Public/README.txt) | Zip contents (EN) |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Modules & flows |
| [`docs/UI_PLAN.md`](docs/UI_PLAN.md) | Operator UI (balances, RPC, MC, Raw) |
| [`docs/CODE_AUDIT.md`](docs/CODE_AUDIT.md) | Code audit (post P0–P2) |
| [`docs/BUGFIX_PLAN.md`](docs/BUGFIX_PLAN.md) | Runtime bugfix status |
| [`docs/RISK_MITIGATION_PLAN.md`](docs/RISK_MITIGATION_PLAN.md) | Residual risks |
| [`docs/IMPROVEMENT_PLAN.md`](docs/IMPROVEMENT_PLAN.md) | Planned features |

### Recent updates (2026-07)

**Hardening**

- Sticky proxy per wallet · L2 gas floors · auth cache flush once  
- RBF multi-hash receipts · integer gwei/fee bumps · type-`LIVE` gate  
- Idle vault auto-lock · OpenSea 429 serial mode · safe share zip  

**Operator UI / RPC**

- Wallet balances **by chain** (incl. Robinhood)  
- **Ping networks** (multi-chain, optional via proxy); private Alchemy only  
- Per-chain Alchemy slugs (no eth-mainnet fallback on L2)  
- **Mission Control** overlay on OpenSea LIVE  
- Raw **Discover**: EIP-1167/1967 proxy resolve + 4byte + explorer ABI  
- Raw multi-wallet Simple `mint(uint256)` documented in ИНСТРУКЦИЯ  

Risk plan: [`docs/RISK_MITIGATION_PLAN.md`](docs/RISK_MITIGATION_PLAN.md) · UI: [`docs/UI_PLAN.md`](docs/UI_PLAN.md)

---

## Safety

- Local only — keys never leave the machine  
- Live mint spends real gas / mint price  
- OpenSea rate limits, RPC quality, and phase timing are outside the app’s control  
- No mint guarantee  
- Do **not** redistribute `keys.vault`, real `config.json`, or `auth_cache.bin`

---

## Author

[X @AndarkFomo](https://x.com/AndarkFomo) · [Telegram](https://t.me/grassfoundationn)

---

## License

MIT OR Apache-2.0
