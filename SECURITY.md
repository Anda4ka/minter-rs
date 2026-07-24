# Security

**MINTER** stores private keys in a local encrypted vault. Treat this machine as the security boundary.

## Product rules

- **Burner wallets only** — never import long-term funds.
- **Never share or commit secrets** — `keys.vault`, `config.json`, `.env`, `auth_cache.bin`, or a live `proxies.txt`.
- Vault password stays in process memory only while unlocked; use **Lock** / idle-lock when away.
- Live mint spends real gas and mint price.
- There is **no mint guarantee** — OpenSea rate limits, RPC quality, and phase timing are outside the app's control.

## Hardening (desktop)

- Tauri **CSP** is set (not `null`) — see `crates/minter-desktop/src-tauri/tauri.conf.json`.
- `Session` **Debug** redacts password, keys, Alchemy, and env values.
- Settings: `config.json` owns RPC/Alchemy; empty fields + Save clear stale `.env` keys.
- **Proxies apply to OpenSea auth, not RPC** — JSON-RPC (tx broadcast, receipts, nonce, balance) is sent direct by design, so your RPC provider sees the machine's real IP.

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |
| < 0.1   | No        |

Only the latest release on `main` / tagged releases receives security fixes.

## Reporting a vulnerability

If you find a vulnerability that can **leak vault keys**, **exfiltrate secrets**, or **forge mint / transfer transactions**, please report it **privately** before any public issue or social post.

**Preferred (in order):**

1. **GitHub Private Vulnerability Reporting** — repository **Security** tab → *Report a vulnerability*  
   (enable under *Settings → Code security* if you are the owner and the button is missing).
2. **Email:** [andarx4nok@gmail.com](mailto:andarx4nok@gmail.com)  
   Subject line: `[SECURITY] minter-rs …`
3. **DM:** [X @AndarkFomo](https://x.com/AndarkFomo) (for a heads-up only; still send technical detail by email or private advisory).

Please include:

- Impact (key leak, tx forgery, RCE, etc.)
- Affected version / commit
- Minimal reproduction steps
- Whether you plan a public write-up (coordinate a date)

You should receive an acknowledgement within **72 hours**. We will work with you on a fix and credit (unless you prefer to stay anonymous).

**Do not** open a public GitHub issue for these classes of bugs.
