# MINTER

Local Windows desktop app for OpenSea drop mints. Multi-wallet sniper with vault, proxies, and task queue.

Built with **Rust** (`minter-core`) + **Tauri 2** (`minter-desktop`). Runs fully offline on your machine — no cloud, no telemetry.

> **Burner wallets only.** Do not use main wallets with real holdings.

---

## Features

- **Encrypted vault** — private keys stay local, password-protected
- **Wallet groups** — A/B/C tags, balance check, proxy per wallet
- **OpenSea drops** — slug/URL, phase picker, WL / eligibility check
- **Mint tasks** — save/edit/duplicate, queue, start/stop
- **Sniper path** — wait for phase open (wall clock) → sim → send → wait for **on-chain confirm**
- **Multi-wallet** — parallel workers, pre-fetch calldata, nonce refresh before open
- **RPC** — Alchemy or custom URLs (Ethereum / Base / Polygon), probe + latency
- **Proxies** — HTTP/SOCKS, health checks
- **Results** — JSON/CSV export, run history, explorer links, full mint logs
- **UI** — EN/RU, phase banner, colored logs, first-confirm badge + optional beep

### Mint flow

```
Start → auth / prep → wait phase open → estimate (sim)
     → if OK: sign + send → wait receipt → CONFIRMED = success
```

`SENT` means the tx was broadcast. **Success is only after block confirmation.**

---

## Stack

| Crate | Role |
|-------|------|
| `minter-core` | Mint engine, OpenSea, vault, RPC, gas, export |
| `minter-desktop` | Tauri GUI |

---

## Build

```powershell
cargo build -p minter-desktop --release
```

Binary: `target/release/minter-desktop.exe`

```powershell
cargo test -p minter-core
cargo check -p minter-core -p minter-desktop
```

---

## Safety

- Local only — keys never leave the machine
- Live mint spends real gas / mint price
- OpenSea rate limits, RPC quality, and phase timing are outside the app’s control
- No mint guarantee

---

## Author

[X @AndarkFomo](https://x.com/AndarkFomo) · [Telegram](https://t.me/grassfoundationn)

---

## License

MIT OR Apache-2.0
