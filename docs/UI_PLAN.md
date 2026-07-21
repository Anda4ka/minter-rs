# UI plan — operator surface

**Audience:** single operator (you).  
**Priority:** speed of reading / acting during mint.  
**Theme:** dark-only forever.  
**Sidebar:** keep all existing nav items — no removal / forced consolidation.

Last update: **2026-07-16** (balances by chain, multi-net RPC probe, Mission Control, Alchemy private-only RPC, Discover proxy, Robinhood).

---

## Shipped UI package

| # | Feature | Status |
|---|---------|--------|
| 1 | Wallets: balance check with **network selector** | **Done** |
| 2 | RPCs: **multi-network ping** with/without first proxy | **Done** |
| 3 | **Mission Control** overlay on live OpenSea mint | **Done** |
| 4 | Robinhood in balance + RPC default set | **Done** |
| 5 | Per-chain Alchemy URL (no eth-mainnet pollution) | **Done** (core) |
| 6 | Raw Discover: EIP-1167 / EIP-1967 proxy resolve | **Done** (core + UI msg) |

---

## 1. Wallets — balances by chain

**UI:** `#wallet-balance-chain` + **Check balances**.

Chains in selector:

`ethereum`, `base`, `polygon`, `arbitrum`, `optimism`, `blast`, `zora`, `apechain`, **`robinhood`**, `monad`.

**Behaviour:**

- Selected wallets, or all if none selected.
- Invokes `wallet_balances` with `{ walletAddresses, chain }`.
- Balance column shows native balance on **that** network.
- Task Start balance filter uses `task.chainOverride` when not `auto`.

---

## 2. RPCs — multi-network ping

**UI:** RPCs page → **Via proxy** + **Ping networks** (+ legacy Probe URL list / Deep latency).

**Command:** `probe_networks({ viaProxy, chains: null })`.

**Default chains:** ethereum, base, polygon, arbitrum, optimism, **robinhood**.

**Table columns:** Chain · Ping · chainId · Path (direct|proxy) · URL · error.

**Core rules (`collect_rpc_urls_for_chain`):**

1. Chain-specific env (`RPC_URL_BASE`, …).
2. **Private Alchemy only:** `https://{slug}.g.alchemy.com/v2/{API_KEY}`.
   - Key from Settings `ALCHEMY_API_KEY` **or** scraped from any pasted `*.g.alchemy.com/v2/<key>` URL.
   - **Never** `*.g.alchemy.com/public`.
3. Non-Alchemy public fallbacks only if no chain URLs yet (publicnode / official RPC — not Alchemy public).
4. Generic `RPC_URL` / `RPC_URLS` attach **only** for ethereum/mainnet — never for L2s (fixed: all L2s were hitting eth-mainnet).

**Alchemy slugs (private):**

| Chain | Slug |
|-------|------|
| ethereum | `eth-mainnet` |
| base | `base-mainnet` |
| polygon | `polygon-mainnet` |
| arbitrum | `arb-mainnet` |
| optimism | `opt-mainnet` |
| blast | `blast-mainnet` |
| zora | `zora-mainnet` |
| apechain | `apechain-mainnet` |
| shape | `shape-mainnet` |
| monad | `monad-mainnet` |
| robinhood | `robinhood-mainnet` |

Docs reference: [Alchemy Chains](https://www.alchemy.com/docs/chains), [RPC directory](https://www.alchemy.com/rpc).

---

## 3. Mission Control overlay

**UI:** `#mission-control` (fixed bottom-right, dark).

- Opens on **Tasks → Start LIVE**.
- Phase strip, stats OK / FAIL / SENT / WAIT / TOTAL.
- Wallet table (cap 80) + log tail (mirrors Tasks).
- **▾** collapse · **Stop** → `cancel_mint`.
- Does not remove Tasks page tables; HUD works while browsing other pages.
- Stays after run ends (manual dismiss not required).

---

## 4. Raw Mint — Discover & multi-wallet (operator notes)

See also full RU steps in `Public/ИНСТРУКЦИЯ.md` § Raw Mint.

### Discover functions

1. Loads bytecode via chain RPC.
2. If **EIP-1167** minimal proxy or **EIP-1967** implementation slot → resolve **implementation**.
3. Sources (merged, mint-like sorted first):
   - explorer ABI (Blockscout-compatible, e.g. Robinhood);
   - hardcoded mint selectors;
   - parallel **4byte.directory** (cap 64, concurrent).
4. Empty list → clear error (proxy note + “paste signature manually”).

Example: `0x9Ec6…` on Robinhood is EIP-1167 → impl `0x73af…` → `mint(uint256)` via `hardcoded@proxy` / 4byte.

### Multi-wallet mint (`mint(uint256)`)

Prefer mode **Simple mint** (not Custom):

| Field | Multi-wallet use |
|-------|------------------|
| Network | Target chain (e.g. Robinhood) |
| Contract | Proxy or impl address (proxy OK) |
| NFTs | qty **per wallet** |
| Pay | ETH **per NFT** (Simple); total value = price × qty |
| Wallets | multi-select; Funded only + Balances |
| Flashbots | **Ethereum only** — off on Robinhood/L2 |
| Dry run | Advanced — sim/pre-sign only |
| **Start** | pre-sign race / T0 blast for **all** selected |
| Send now | one-shot without wait |

**Custom mode:** Function `mint(uint256)`, Params `1`, Pay = **total** ETH per call.

Mental model: N wallets ⇒ N transactions, same calldata shape, different `from`/nonce.

---

## Out of scope (this UI window)

- New OpenSea mint protocol features.
- Light theme.
- Removing sidebar entries.
- Full Settings redesign.

---

## Optional polish (later)

- Hotkey reopen Mission Control / Esc collapse.
- Per-chain checkboxes for RPC ping.
- Persist last balance chain.
- Mission Control on Raw Start.
- Command palette (Ctrl+K).

---

## Verify

```powershell
cargo test -p minter-core
cargo check -p minter-desktop
# UI static files: restart app after package
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
```

**Backend commands:** `wallet_balances`, `probe_networks`, `discover_raw_functions`, `raw_mint`, `raw_sniper`, `run_mint`.
