# MINTER — архитектура

Краткий map репозитория для разработчиков.  
**Runtime для оператора:** только `Public\minter-desktop.exe`.

## Canonical repository

| | |
|--|--|
| **URL** | https://github.com/Anda4ka/minter-rs |
| **Branch** | `main` |
| **Push** | **Только в этот репозиторий.** Не пушить product-коммиты в `Anda4ka/MINTER` / `Minter-privat` без явного решения. |

Local remotes may be named `origin` or `viktor` — always check `git remote -v` points at **minter-rs**.

---

## Workspace

```
minter-rs/                   ← clone of github.com/Anda4ka/minter-rs
  Cargo.toml                 workspace
  crates/minter-core/        shared engine (lib)
  crates/minter-desktop/     Tauri 2 app + UI
  scripts/package-public.ps1 release → Public\
  Public/                    shipped product + local data (mostly gitignored)
  docs/                      plans, audit, this file
```

| Crate | Edition | Role |
|-------|---------|------|
| `minter-core` | 2024 | Mint, vault, RPC, OpenSea, sniper, export |
| `minter-desktop` | 2021 | Tauri commands, window, shell open |

---

## Runtime layout (`Public\`)

| Path | Who writes | Ship in zip? |
|------|------------|--------------|
| `minter-desktop.exe` | package script | **yes** |
| `config.example.json` | repo | yes |
| `proxies.example.txt` | repo | yes |
| `ИНСТРУКЦИЯ.md` / `README.txt` | repo | yes |
| `config.json` | app Settings | **no** |
| `keys.vault` | app Wallets | **no** |
| `tasks.json`, `wallet_meta.json` | app | **no** |
| `auth_cache.bin` | app (encrypted SIWE) | **no** |
| `results/`, `logs/` | app mint export | **no** |

`target\` — only Cargo build cache; not required to run Public exe.

---

## Module map (`minter-core`)

| Module | Responsibility |
|--------|----------------|
| `vault` | AES-GCM encrypt keys, atomic write |
| `settings` | `config.json` load/save, env migration |
| `auth_cache` | SIWE tokens, memory save + `flush` |
| `opensea` | SIWE, drop info, GQL mint calldata, public local build |
| `mint` | OpenSea multi-wallet orchestration |
| `raw_sniper` | Pre-sign race (custom / simple mint) |
| `raw_mint` | Raw contract mint + **Discover** (proxy resolve) |
| `rpc` | JSON-RPC client, optional HTTP proxy, race send, multi-hash receipt, `get_storage_at` |
| `gas` | fees, L2 floors, `bump_fee_bps`, gwei→wei |
| `sign` | EIP-1559 sign |
| `proxy` | parse + sticky assignment |
| `api` / `Session` | high-level UI API (`wallet_balances`, `probe_networks`, …) |
| `flashbots` | mainnet bundles only |
| `disperse` / `sweep` / `multicall` | funding & batch utils |
| `export` / `progress` | results + mint events (WL export, Mission Control feed) |
| `amount` / `abi` / `types` | pure helpers (selectors, EIP-1167 parse, 4byte) |

---

## OpenSea mint flow

```
unlock vault
  → Session.run_opensea_mint
    → auth (cache + SIWE, flush once)
    → phase pick / per-wallet qty
    → wall-clock wait (prefetch T−5s, nonce T−2s)
    → workers: calldata → estimate/gas → sign → send
    → wait_for_any_receipt (RBF candidates)
    → CONFIRMED | FAILED | SENT(timeout)
    → optional export JSON/CSV + history
```

**Cancel:** `AtomicBool` checked in wait + between attempts.

---

## Raw sniper flow

```
resolve calldata + value (simple mint(uint256) or custom signature)
  → wait until at_time − 5s
  → pre-sign all (nonce + fees)
  → wait until at_time (ms spin)
  → if chainId=1: fee_history; re-sign if fees rose
  → parallel eth_sendRawTransaction
  → receipts off hot path
```

### Discover functions (`discover_raw_functions`)

```
eth_getCode(contract)
  → EIP-1167 minimal proxy? or EIP-1967 impl slot?
  → eth_getCode(implementation)
  → extract PUSH4 selectors
  → explorer ABI (Blockscout) ∪ hardcoded mint ∪ 4byte (parallel)
  → sort mint-like first → UI select
```

Multi-wallet: **N wallets ⇒ N txs** with the same function/params; value/gas per wallet.

---

## RPC URL resolution

`collect_rpc_urls_for_chain(env, chain, extra)` order:

1. Extra URLs  
2. Chain-specific env (`RPC_URL_BASE`, `BASE_RPC_URL`, …)  
3. **Private Alchemy** only: `https://{slug}.g.alchemy.com/v2/{key}`  
   - Key: Settings field **or** scraped from any `*.g.alchemy.com/v2/<key>` URL  
   - **Never** Alchemy `/public`  
4. Non-Alchemy public fallbacks (if still empty)  
5. Generic `RPC_URL` / `RPC_URLS` **only** when chain is ethereum/mainnet/empty  

`Session.probe_networks` pings default set: ethereum, base, polygon, arbitrum, optimism, robinhood (optional via first proxy).

---

## Gas policy

| Path | Behaviour |
|------|-----------|
| OpenSea estimate | `apply_gas_limit` (L2 floor 150k) |
| OpenSea fixed / skip preflight | clamp up on elevated chains |
| Raw sniper | hard `gas_limit` (default 650k), no estimate at T0 |
| Fee bump | integer bps (11500 / 13000) |

Elevated chains: OP, Base, Arb, Nova, Blast, Zora, Ape, Shape, MegaETH, Robinhood, Monad, …

---

## Desktop (`minter-desktop`)

- `AppState`: `Session`, `mint_running`, `mint_cancel`, first-confirm gate.
- Commands: unlock, wallets, settings, `run_mint`, `cancel_mint`, `wallet_balances`, `probe_networks`, `discover_raw_functions`, raw/sweep/disperse, …
- Events: `mint-event`, `mint-first-confirm`, `mint-reauth`.
- UI: `ui/app.js` + `i18n.js` (EN/RU) + dark-only `styles.css`.
- **Mission Control** (`#mission-control`): live HUD on OpenSea Start LIVE (phase, stats, wallet rows, log mirror).

---

## Build & ship

```powershell
# from repo root
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
.\Public\minter-desktop.exe
# optional free disk:
Remove-Item -Recurse -Force target
```

Tests:

```powershell
cargo test -p minter-core
```

---

## Related docs

- `docs/UI_PLAN.md` — operator UI (balances, RPC probe, Mission Control, Raw Discover)  
- `docs/CODE_AUDIT.md` — findings after P0–P2  
- `docs/BUGFIX_PLAN.md` — fix history  
- `docs/RISK_MITIGATION_PLAN.md` — residual risks  
- `docs/IMPROVEMENT_PLAN.md` — hybrid open, funding, metrics  
- `Public/ИНСТРУКЦИЯ.md` — end-user guide (RU)  

