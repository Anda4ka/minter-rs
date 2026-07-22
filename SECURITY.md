# Security

**MINTER** stores private keys in a local encrypted vault. Treat this machine as the security boundary.

## Canonical repository

- **https://github.com/Anda4ka/minter-rs** only for product source / push
- Do not commit `keys.vault`, `config.json`, `.env`, `auth_cache.bin`, or live `proxies.txt`

## Product rules

- **Burner wallets only** — never import long-term funds
- Vault password stays in process memory while unlocked; use **Lock** / idle lock when idle
- Live mint spends real gas and mint price

## Hardening (desktop)

- Tauri **CSP** is set (not `null`) — see `crates/minter-desktop/src-tauri/tauri.conf.json`
- `Session` **Debug** redacts password, keys, Alchemy, and env values
- Settings: `config.json` owns RPC/Alchemy; empty fields + Save clear stale `.env` keys

## Reporting

If you find a vulnerability that can leak vault keys or forge mint txs, contact the maintainer privately (see README author links). Prefer responsible disclosure before public issues.
