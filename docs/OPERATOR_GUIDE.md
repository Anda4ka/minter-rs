# MINTER — Operator guide (EN)

Short first-run guide for the Windows desktop app. Full walkthrough in Russian: [`USER_GUIDE.md`](../USER_GUIDE.md).

**Version:** 0.1.0  
**Author:** [X @AndarkFomo](https://x.com/AndarkFomo) · [Telegram](https://t.me/grassfoundationn)  
**Source:** [github.com/Anda4ka/minter-rs](https://github.com/Anda4ka/minter-rs)

---

## What it is

Local **Windows** GUI for:

- OpenSea **SeaDrop** mints (multi-wallet, phase wait, fixed-gas LIVE send)
- **Raw contract** sniping (`mint(uint256)` and discovered ABI paths)
- Vault, proxies, RPC health, WL check, results/logs export

| Yes | No |
|-----|-----|
| Runs only on your PC | Cloud / telemetry |
| Burner wallets | Main wallets with life savings |
| Keys in encrypted vault | Keys in Discord / chat |
| Settings in `config.json` next to the exe | Required hand-edited `.env` (legacy; UI config wins) |

**Rules**

1. **Burner wallets only**
2. Live mint spends **real** gas + mint price
3. OpenSea **Tasks → Start is LIVE** — by default type `LIVE` to confirm
4. Success = on-chain **CONFIRMED**, not `SENT` (broadcast only)

Not affiliated with OpenSea. You are responsible for keys, funds, and compliance with applicable terms and laws. No mint guarantee.

---

## Install

### Option A — Release zip

1. Download `minter-desktop-*-windows.zip` from [Releases](https://github.com/Anda4ka/minter-rs/releases)
2. Verify SHA256 if a `.sha256` file is provided
3. Unzip to a folder (e.g. `C:\Minter\`)
4. Run `minter-desktop.exe`  
   SmartScreen (“Unknown publisher”) → **More info** → **Run anyway** (unsigned builds)

### Option B — From source

Prerequisites: Rust (stable), MSVC C++ build tools, WebView2.

```powershell
git clone https://github.com/Anda4ka/minter-rs
cd minter-rs
cargo run -p minter-desktop --release
```

Binary: `target\release\minter-desktop.exe`

---

## First run (checklist)

1. **Vault** — accept burner warning, set password, **Unlock**
2. **Wallets** — import burner keys (paste / file; one key per line). Keys are never shown in the table
3. **Settings → Connection** — paste your **Alchemy** API key (private; multi-chain URLs are built for you) → **Save**
4. **RPCs → Ping networks** — confirm chainIds look right (Base ≈ 8453, Ethereum = 1, …)
5. **Proxies** — paste one per line (`host:port:user:pass`, `socks5://…`, etc.) and save  
   Multi-wallet OpenSea without proxies often hits HTTP 429
6. Keep **Dry Run** habits for raw/sweep; for OpenSea Tasks, Start is always LIVE (type `LIVE`)

---

## OpenSea mint flow

1. **Tasks → Create task**
2. Collection **slug** or OpenSea URL → **Load phases** → pick phase
3. Quantity, gas / priority fee, optional **At time**
4. Select wallets (groups A/B/C); prefer **only funded wallets**
5. **Save** → **Start** → type **`LIVE`**
6. App: prep/auth → wait phase open (wall clock) → fixed-gas sign/send → wait **receipt**
7. **CONFIRMED** = success. Watch **Mission Control** HUD during the run

`SENT` means broadcast only — not a win until confirmed in a block.

---

## Raw mint (multi-wallet race)

Sidebar **Raw Mint** (not Tasks):

1. Network + contract (`0x…`; proxies EIP-1167/1967 resolved when possible)
2. Simple mode: qty per wallet, ETH price per NFT, wallets, gas limit, optional fire timestamp
3. Advanced → **Dry run** first
4. **Start** / **Send now**

Flashbots: Ethereum mainnet only.

---

## Files next to the exe (do not share)

| Path | Contents |
|------|----------|
| `keys.vault` | Encrypted private keys |
| `config.json` | RPC / gas / safety settings |
| `tasks.json`, `wallet_meta.json`, `runs_history.json` | Local state |
| `proxies.txt`, `auth_cache.bin` | Proxies + OpenSea SIWE cache |
| `results/`, `logs/` | Exports and full mint logs |

Share only the exe + docs — never the files above.

---

## Troubleshooting (short)

| Symptom | Try |
|---------|-----|
| Exe blocked | SmartScreen → Run anyway; antivirus exclusion |
| Vault locked | Unlock with password |
| RPC / wrong chainId | Settings → Save → Ping networks |
| 401 / 429 OpenSea | Proxies, slow down, warm auth |
| intrinsic gas too low (L2) | Raise gas limit / Auto; app floors elevated L2 |
| Start blocked | Read reason on task card (slug / wallets / RPC / vault) |

---

## Rebuild / local package

```powershell
# from repo root
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
# optional safe zip (no secrets):
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1 -MakeZip
```

`Public\` is **gitignored** local output. Official binaries come from GitHub Releases (tag `v*`).

---

**Bottom line:** folder → exe → Unlock → wallets → Alchemy + proxies → task → type **LIVE** → wait open → **CONFIRMED**.

Do not mint from a main wallet.
