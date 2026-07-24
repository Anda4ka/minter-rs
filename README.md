<!-- Hero -->
<p align="center">
  <img src=".github/assets/hero.svg" width="100%" alt="MINTER — OpenSea drop mints and raw contract sniping. Rust engine, Tauri 2 desktop, Windows. Pipeline: wait, sign, send, confirmed.">
</p>

<!-- Badges -->
<p align="center">
  <a href="https://github.com/Anda4ka/minter-rs/actions/workflows/ci.yml"><img src="https://github.com/Anda4ka/minter-rs/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/Anda4ka/minter-rs/releases/latest"><img src="https://img.shields.io/github/v/release/Anda4ka/minter-rs?style=flat-square&label=release" alt="Release"></a>
  <img src="https://img.shields.io/badge/engine-Rust-000000?style=flat-square&logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/desktop-Tauri%202-0a101c?style=flat-square&logo=tauri&logoColor=57c06b" alt="Tauri 2">
  <img src="https://img.shields.io/badge/platform-Windows-0a101c?style=flat-square&logo=windows&logoColor=white" alt="Windows">
  <img src="https://img.shields.io/badge/wallets-burner%20only-cf4b2f?style=flat-square" alt="Burner wallets only">
  <img src="https://img.shields.io/badge/telemetry-none-3fa652?style=flat-square" alt="No telemetry">
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
  <a href="#-install">Install</a> ·
  <a href="#-build--run">Build &amp; run</a> ·
  <a href="#-configure">Configure</a> ·
  <a href="#-safety">Safety</a> ·
  <a href="#-docs">Docs</a>
</p>

> [!WARNING]
> **Burner wallets only.** Never import long-term funds. Live mint spends real gas and mint price, and there is **no mint guarantee** — OpenSea rate limits, RPC quality, and phase timing are outside the app's control.
>
> **Not affiliated with OpenSea.** You are solely responsible for compliance with OpenSea’s terms, chain rules, and applicable law. Software is provided **as-is**, without warranty.

<br>

## ⚡ Quick start

### Release binary (Windows)

1. Download the latest **`minter-desktop-*-windows.zip`** from [Releases](https://github.com/Anda4ka/minter-rs/releases)
2. Verify the SHA256 checksum when provided
3. Unzip → run `minter-desktop.exe`
4. Settings → Alchemy API key → Proxies → **Check Connection**
5. **Dry Run** habits for raw tools; OpenSea **Tasks → Start** is LIVE (type `LIVE`)

### From source

```powershell
git clone https://github.com/Anda4ka/minter-rs
cd minter-rs
cargo run -p minter-desktop --release
```

**First run:** open **Settings** → paste your **Alchemy API key** → **Proxies** → paste your list → **Check Connection**. Prerequisites in [Build &amp; run](#-build--run) · first-run setup in [Configure](#-configure) · short EN walkthrough in [`docs/OPERATOR_GUIDE.md`](docs/OPERATOR_GUIDE.md).

<br>

<!-- 01 -->
<h2 id="-overview"></h2>
<img src=".github/assets/section-overview.svg" width="100%" alt="Overview — what it is">

MINTER is a **self-contained desktop app**: a Rust engine (`minter-core`) wrapped in a Tauri 2 GUI (`minter-desktop`). You import burner keys into an encrypted vault, pick a drop (by OpenSea slug/URL or raw contract address), and fire from many wallets at phase open.

- **No cloud, no telemetry** — keys, config, tasks and results stay on the machine.
- **Encrypted at rest** — AES-256-GCM vault, PBKDF2 (600k), atomic writes, `Zeroizing` in memory.
- **Confirm-based success** — a broadcast (`SENT`) is never counted as a win; only a block-confirmed receipt is.
- **Two mint paths** — OpenSea SeaDrop drops, and raw contract sniping with proxy/ABI discovery.
- **Windows only** for the GUI. `minter-core` tests also run on Linux CI.

> 📖 Operator guides: **[EN](docs/OPERATOR_GUIDE.md)** · **[RU full](USER_GUIDE.md)**

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
| **Advanced** | Sweep ETH/NFT, disperse, multicall helpers; Flashbots path on **Ethereum mainnet only** |
| **RPC** | Private Alchemy multi-chain (your key only) · **Ping networks** (+ via proxy) · latency |
| **Proxies** | HTTP / SOCKS, health checks, sticky wallet mapping (OpenSea auth path) |
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
<h2 id="-install"></h2>

| Method | When |
|--------|------|
| **[GitHub Releases](https://github.com/Anda4ka/minter-rs/releases)** | You want a zip + `minter-desktop.exe` |
| **Build from source** | You develop or want a custom build |

Unsigned builds may show Windows SmartScreen — **More info → Run anyway**. Code signing is not included in v0.1.x.

<br>

<!-- 05 -->
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

**Tests (core):** `cargo test -p minter-core --lib`

**Local ship folder** (gitignored `Public\`, secrets never packed):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
```

> The app writes its data (vault, `config.json`, results, logs) **next to the exe** — those are gitignored; never share them.

<br>

<!-- 06 -->
<h2 id="-configure"></h2>
<img src=".github/assets/section-configure.svg" width="100%" alt="Configure — first run">

Set connection details in the **Settings** UI (saved to `config.json` next to the exe).

**First run**

1. **Unlock / create the vault** — set a password, then import burner keys (paste or file).
2. **RPC** — in **Settings**, paste your **Alchemy API key** (private key; multi-chain URLs are built for you). Advanced: set explicit `rpc_url_ethereum` / `rpc_url_base` / `rpc_urls`.
3. **Proxies** — on the **Proxies** page, paste one per line (formats below). Multi-wallet OpenSea without proxies often hits HTTP 429.
4. **Check Connection** — **Ping networks** to confirm RPC + proxies are healthy.
5. **Dry Run is ON by default** (global chip for raw/sweep). OpenSea **Tasks → Start** is always LIVE — type `LIVE` when prompted.

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

<!-- 07 -->
<h2 id="-safety"></h2>
<img src=".github/assets/section-safety.svg" width="100%" alt="Safety — read before going live">

- **Local only** — keys never leave the machine (local vault / RPC / OpenSea SIWE only).
- **Burner wallets only** — never import long-term funds.
- **Live mint spends real value** — gas + mint price, every time.
- **No mint guarantee** — OpenSea rate limits, RPC quality, and phase timing are outside the app's control.
- **Keep `LIVE` confirm + idle-lock on** — type `LIVE` to start a live run; the vault auto-locks when idle.
- **Never redistribute** `keys.vault`, a real `config.json`, or `auth_cache.bin`.
- **Proxies ≠ RPC privacy** — proxies cover OpenSea HTTP auth; JSON-RPC goes direct (provider sees your IP).
- **No warranty** — provided **as-is**. **You are solely responsible** for your use, your funds, and compliance with any applicable terms and laws.

Security reports: see **[SECURITY.md](SECURITY.md)** (private disclosure).

<br>

<!-- 08 -->
<h2 id="-docs"></h2>

| Doc | Audience |
|-----|----------|
| [`docs/OPERATOR_GUIDE.md`](docs/OPERATOR_GUIDE.md) | EN first-run + mint flow |
| [`USER_GUIDE.md`](USER_GUIDE.md) | Full RU operator manual |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Build, PR, tests |
| [`SECURITY.md`](SECURITY.md) | Vulnerability reporting |
| [`CHANGELOG.md`](CHANGELOG.md) | Release history |
| [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) | Community standards |

<br>

## Project layout

```text
crates/minter-core/          # mint engine, vault, RPC, OpenSea, raw sniper
crates/minter-desktop/       # Tauri 2 app + static UI
  src-tauri/                 # Rust shell
  ui/                        # HTML/CSS/JS
scripts/package-public.ps1   # local Windows ship folder / safe zip
.github/workflows/           # CI + tag release
```

<br>

## Contributing

PRs welcome for fixes, docs, and focused features. Please read [CONTRIBUTING.md](CONTRIBUTING.md).  
By contributing you agree to dual-license your work under **MIT OR Apache-2.0**.

<br>

---

<p align="center">
  <sub>
    <b>MINTER</b> · Rust + Tauri 2 · Windows ·
    <a href="https://x.com/AndarkFomo">X @AndarkFomo</a> ·
    <a href="https://t.me/grassfoundationn">Telegram</a> ·
    <a href="docs/OPERATOR_GUIDE.md">EN guide</a> ·
    <a href="USER_GUIDE.md">RU guide</a><br>
    Licensed under <a href="LICENSE-MIT">MIT</a> OR <a href="LICENSE-APACHE">Apache-2.0</a>
    · <a href="NOTICE">NOTICE</a>
  </sub>
</p>
