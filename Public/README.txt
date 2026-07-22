MINTER — Windows GUI (Tauri) v0.1.0
====================================

Source / only git push target:
  https://github.com/Anda4ka/minter-rs   (branch: main)
  Do not push product updates to other GitHub repos by default.

Program folder should contain ONLY:
  minter-desktop.exe    - Main app (release build)
  config.example.json   - Settings template
  proxies.example.txt   - Proxy list format
  ИНСТРУКЦИЯ.md         - Full guide (Russian) — read this
  README.txt            - this file

Do NOT ship: keys.vault, config.json, proxies.txt, tasks.json,
  runs_history.json, wallet_meta.json, auth_cache.bin, results/, logs/

Burner wallets only. No telemetry. Keys stay in local encrypted vault.

Quick start:
  1. Put the files above in a folder (e.g. C:\Minter)
  2. Double-click minter-desktop.exe
  3. Accept burner warning, set vault password, Unlock
  4. Wallets → import burners
     (optional) pick Network + Check balances
  5. Settings → Alchemy API key (private endpoints only) or RPC → Save
  6. RPCs → Ping networks (each chain should show its own chainId)
  7. (Optional) Proxies → save list
  8. Tasks → Create task (slug → phase → wallets) → Start
     → LIVE (type LIVE by default)
     → Mission Control HUD appears bottom-right
     → wait phase open → fixed gas send → on-chain CONFIRMED
  9. Raw Mint (sidebar) → multi-wallet contract mint
     → Simple mint = mint(uint256) × N wallets
     → Discover resolves EIP-1167/1967 proxies
     → Full steps: ИНСТРУКЦИЯ.md § Raw Mint

Success = CONFIRMED in a block (SENT alone is not success).
Phase open uses wall-clock time (not waiting for next chain block).
OpenSea LIVE uses fixed gas (no eth_estimateGas gate on open).

Notes (2026-07):
  - Canonical repo: github.com/Anda4ka/minter-rs only for push
  - Private Alchemy only (no Alchemy /public URLs)
  - Per-chain RPC (L2 never falls back to eth-mainnet silently)
  - Wallet balances by network (incl. Robinhood)
  - config.json owns RPC/Alchemy (empty + Save clears stale .env)
  - Mission Control overlay on OpenSea LIVE
  - Raw Discover: proxy implementation + 4byte + explorer ABI
  - Sticky per-wallet proxy · L2 gas floors · type-LIVE gate
  - RBF multi-hash receipts

After first run the app may create next to the exe:
  keys.vault, config.json, tasks.json, wallet_meta.json,
  runs_history.json, proxies.txt, auth_cache.bin, results/, logs/

Never redistribute vault, real config, proxies, or your results/logs.

Developers (rebuild into this folder from repo root):
  powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
  (target\ is build cache only — not needed to run this exe)

Creator: https://x.com/AndarkFomo  ·  https://t.me/grassfoundationn

Full steps (Russian): open ИНСТРУКЦИЯ.md
