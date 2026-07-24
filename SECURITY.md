# Security

**MINTER** stores private keys in a local encrypted vault. Treat this machine as the security boundary.

## Product rules

- **Burner wallets only** — never import long-term funds.
- **Never share or commit secrets** — `keys.vault`, `config.json`, `.env`, `auth_cache.bin`, or a live `proxies.txt`.
- Vault password stays in process memory only while unlocked; use **Lock** / idle-lock when away.
- Live mint spends real gas and mint price.

## Hardening (desktop)

- Tauri **CSP** is set (not `null`) — see `crates/minter-desktop/src-tauri/tauri.conf.json`.
- `Session` **Debug** redacts password, keys, Alchemy, and env values.
- Settings: `config.json` owns RPC/Alchemy; empty fields + Save clear stale `.env` keys.
- **Proxies apply to OpenSea auth, not RPC** — JSON-RPC (tx broadcast, receipts, nonce, balance) is sent direct by design, so your RPC provider sees the machine's real IP.

## Reporting

If you find a vulnerability that can leak vault keys or forge mint transactions, please report it privately to the maintainer (see the author links in the README) before opening a public issue.
