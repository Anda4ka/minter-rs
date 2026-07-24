<!-- Hero -->
<p align="center">
  <img src=".github/assets/hero.svg" width="100%" alt="MINTER — OpenSea drop mints and raw contract sniping. Rust engine, Tauri 2 desktop, Windows. Pipeline: wait, sign, send, confirmed.">
</p>

<!-- Badges -->
<p align="center">
  <a href="https://github.com/Anda4ka/minter-rs/actions/workflows/ci.yml"><img src="https://github.com/Anda4ka/minter-rs/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/engine-Rust-000000?style=flat-square&logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/desktop-Tauri%202-0a101c?style=flat-square&logo=tauri&logoColor=57c06b" alt="Tauri 2">
  <img src="https://img.shields.io/badge/platform-Windows-0a101c?style=flat-square&logo=windows&logoColor=white" alt="Windows">
  <img src="https://img.shields.io/badge/wallets-burner%20only-cf4b2f?style=flat-square" alt="Burner wallets only">
  <img src="https://img.shields.io/badge/telemetry-none-3fa652?style=flat-square" alt="No telemetry">
  <img src="https://img.shields.io/badge/core%20tests-190%20passing-3fa652?style=flat-square" alt="190 core tests passing">
  <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-3fa652?style=flat-square" alt="License MIT or Apache-2.0">
</p>

<p align="center">
  <b>A local Windows desktop tool for OpenSea drop mints and raw-contract sniping.</b><br>
  Multi-wallet, encrypted vault, proxies, task queue, results export — with one rule that shapes everything:<br>
  <b>success is an on-chain <code>CONFIRMED</code> receipt, not a broadcast.</b>
</p>

<p align="center">
  <a href="#-overview">Overview</a> ·
  <a href="#-features">Features</a> ·
  <a href="#-how-it-mints">How it mints</a> ·
  <a href="#-build--run">Build &amp; run</a> ·
  <a href="#-configure">Configure</a> ·
  <a href="#-safety">Safety</a>
</p>

> [!WARNING]
> **Burner wallets only.** Never import long-term funds. Live mint spends real gas and mint price, and there is **no mint guarantee** — OpenSea rate limits, RPC quality, and phase timing are outside the app's control.

<br>

## ⚡ Quick start

Build and run from source in one command (Windows, from the repo root):

```powershell
git clone https://github.com/Anda4ka/minter-rs
cd minter-rs
cargo run -p minter-desktop --release
```

**First run:** open **Settings** → paste your **Alchemy API key** → **Proxies** → paste your list → **Check Connection**. **Dry Run is ON by default** — flip to LIVE only when you're ready (type `LIVE`). Prerequisites in [Build &amp; run](#-build--run) · first-run setup in [Configure](#-configure).

<br>

<!-- 01 -->
<h2 id="-overview"></h2>
<img src=".github/assets/section-overview.svg" width="100%" alt="Overview — what it is">

MINTER is a **self-contained desktop app**: a Rust engine (`minter-core`) wrapped in a Tauri 2 GUI (`minter-desktop`). You import burner keys into an encrypted vault, pick a drop (by OpenSea slug/URL or raw contract address), and fire from many wallets at phase open.

- **No cloud, no telemetry** — keys, config, tasks and results stay on the machine.
- **Encrypted at rest** — AES-256-GCM vault, PBKDF2 (600k), atomic writes, `Zeroizing` in memory.
- **Confirm-based success** — a broadcast (`SENT`) is never counted as a win; only a block-confirmed receipt is.
- **Two mint paths** — OpenSea SeaDrop drops, and raw contract sniping with proxy/ABI discovery.

> 📖 Full step-by-step operator guide (RU): **[`USER_GUIDE.md`](USER_GUIDE.md)**.

<br>

<!-- 02 -->
<h2 id="-features"></h2>
<img src=".github/assets/section-features.svg" width="100%" alt="Features — what it does">

| Area | What you get |
|------|--------------|
| **Vault** | AES-GCM encrypted keys, password-protected, atomic writes, memory zeroized on lock |
| **Wallets** | Import, A/B/C groups, **balances by network**, per-wallet sticky proxy |
| **OpenSea drops** | Slug/URL resolve, phase picker, WL / eligibility export |
| **Tasks** | slug → phase → wallets → **Start** (LIVE; type `LIVE` to confirm by default) |
| **Mission Control** | Live HUD on OpenSea Start — phase, stats, per-wallet rows, mirrored log |
| **OpenSea mint** | Wall-clock phase open → **fixed-gas** send (no estimate gate on LIVE) → on-chain confirm |
| **Raw Mint** | Multi-wallet pre-sign race · Discover (EIP-1167/1967 proxy + 4byte) · simple `mint(uint256)` |
| **RPC** | Private Alchemy multi-chain (your key only) · **Ping networks** (+ via proxy) · latency |
| **Proxies** | HTTP / SOCKS, health checks, sticky wallet mapping |
| **Results** | JSON / CSV export, run history, explorer links, full mint logs |
| **UI** | Dark-only, EN / RU, phase banner, first-confirm badge + optional beep |

<br>

<!-- 03 -->
<h2 id="-how-it-mints"></h2>
<img src=".github/assets/section-flow.svg" width="100%" alt="How it mints — success equals on-chain confirm">

<p align="center">
  <img src=".github/assets/flow.svg" width="100%" alt="OpenSea live mint flow: unlock, auth (SIWE + cache), wait open (wall-clock), fixed gas + sign, race send (multi-RPC), CONFIRMED. Raw sniper flow: resolve calldata + value, pre-sign all at T minus 5s, clock fire, blast parallel send, receipts off hot-path.">
</p>

**The rule:** `SENT` = broadcast only. **A mint is a win only after block confirmation.** Receipts are polled across all RBF replacement hashes, so a fee bump can't lose the result.

- **OpenSea LIVE** opens on the **wall clock** (not `block.timestamp`), sends with **fixed gas** (no `eth_estimateGas` gate on live), and decodes SeaDrop `NotActive` (`0x13da22f2`) near open for clear logs.
- **Raw sniper** resolves calldata + value, **pre-signs** every wallet at T−5s, fires on a millisecond clock, and blasts in parallel — receipts are kept off the hot path. On mainnet, fees are re-signed at fire.

<br>

<!-- 04 -->
<h2 id="-build--run"></h2>
<img src=".github/assets/section-build.svg" width="100%" alt="Build and run — Windows, PowerShell">

**Prerequisites (Windows):** **Rust** (stable — [rustup.rs](https://rustup.rs)) · **MSVC C++ build tools** ("Desktop development with C++") · **WebView2** (preinstalled on Windows 10/11). _No Node.js — the UI is static and served by Tauri._

**Build & run — one command** (from the repo root):

```powershell
cargo run -p minter-desktop --release
```

**Optimized binary** → `target\release\minter-desktop.exe`:

```powershell
cargo build -p minter-desktop --release
```

**Tests:** `cargo test -p minter-core`

> The app writes its data (vault, `config.json`, results, logs) **next to the exe** — those are gitignored; never share them.

<br>

<!-- 05 -->
<h2 id="-configure"></h2>
<img src=".github/assets/section-configure.svg" width="100%" alt="Configure — first run">

Set connection details in the **Settings** UI (saved to `config.json` next to the exe).

**First run**

1. **Unlock / create the vault** — set a password, then import burner keys (paste or file).
2. **RPC** — in **Settings**, paste your **Alchemy API key** (private key; multi-chain URLs are built for you). Advanced: set explicit `rpc_url_ethereum` / `rpc_url_base` / `rpc_urls`.
3. **Proxies** — on the **Proxies** page, paste one per line (formats below). Multi-wallet OpenSea without proxies often hits HTTP 429.
4. **Check Connection** — **Ping networks** to confirm RPC + proxies are healthy.
5. **Dry Run is ON by default** — do a dry pass first; switch to LIVE only when ready (type `LIVE`).

**Proxy formats**

```text
host:port
host:port:user:pass
user:pass@host:port
http://user:pass@host:port
socks5://host:port
```

Prefer env / headless config? Copy [`.env.example`](.env.example) → `.env` (`ALCHEMY_API_KEY`, `RPC_URL(S)_<CHAIN>`). `config.json` (Settings) takes precedence; empty fields + **Save** clear stale `.env` keys.

<br>

<!-- 06 -->
<h2 id="-safety"></h2>
<img src=".github/assets/section-safety.svg" width="100%" alt="Safety — read before going live">

- **Local only** — keys never leave the machine (local vault / RPC / OpenSea SIWE only).
- **Burner wallets only** — never import long-term funds.
- **Live mint spends real value** — gas + mint price, every time.
- **No mint guarantee** — OpenSea rate limits, RPC quality, and phase timing are outside the app's control.
- **Keep `LIVE` confirm + idle-lock on** — type `LIVE` to start a live run; the vault auto-locks when idle.
- **Never redistribute** `keys.vault`, a real `config.json`, or `auth_cache.bin`.
- **No warranty** — provided **as-is**, without warranty of any kind. **You are solely responsible** for your use, your funds, and compliance with any applicable terms and laws.

<br>

---

<p align="center">
  <sub>
    <b>MINTER</b> · Rust + Tauri 2 · <a href="https://x.com/AndarkFomo">X @AndarkFomo</a> · <a href="https://t.me/grassfoundationn">Telegram</a> · <a href="USER_GUIDE.md">User guide</a><br>
    Licensed under <b>MIT OR Apache-2.0</b>
  </sub>
</p>
