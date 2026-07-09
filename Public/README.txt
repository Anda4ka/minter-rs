MINTER — Windows GUI (Tauri) v0.1.0
====================================

Public zip should contain ONLY:
  minter-desktop.exe    - Main app (release build)
  config.example.json   - Settings template
  proxies.example.txt   - Proxy list format
  ИНСТРУКЦИЯ.md         - Full guide (Russian) — read this
  README.txt            - this file

Do NOT ship: keys.vault, config.json, proxies.txt, tasks.json,
  runs_history.json, wallet_meta.json, auth_cache.bin, results/, logs/

Burner wallets only. No telemetry. Keys stay in local encrypted vault.

Quick start:
  1. Put the 5 files above in a folder (e.g. C:\Minter)
  2. Double-click minter-desktop.exe
  3. Accept burner warning, set vault password, Unlock
  4. Wallets → import burners
  5. Settings → Alchemy or RPC → Save
  6. RPCs → Probe (must be OK)
  7. (Optional) Proxies → save list
  8. Tasks → Create task → Start
     → LIVE immediately (no typing CONFIRM)
     → sim → if OK send tx → wait for on-chain CONFIRMED

Success = CONFIRMED in a block (SENT alone is not success).
Phase open uses wall-clock time (not waiting for next chain block).

After first run the app may create next to the exe:
  keys.vault, config.json, tasks.json, wallet_meta.json,
  runs_history.json, proxies.txt, auth_cache.bin, results/, logs/

Never redistribute vault, real config, proxies, or your results/logs.

Creator: https://x.com/AndarkFomo  ·  https://t.me/grassfoundationn

Full steps (Russian): open ИНСТРУКЦИЯ.md
