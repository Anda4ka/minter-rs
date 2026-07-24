# Contributing to MINTER

Thanks for taking an interest in the project. This is a **Windows-first** desktop app (Rust + Tauri 2). Most users run a release binary; contributors usually work from source.

## Ground rules

- **Burner wallets only** in any test or demo — never commit keys, vaults, `config.json`, proxies with credentials, or `.env` files that hold real secrets.
- Prefer small, focused PRs over large multi-purpose ones.
- Keep the product local: no telemetry, no phone-home, no cloud key storage.
- Match existing style (Rustfmt + Clippy clean on `minter-core`).

## Prerequisites (Windows)

- [Rust](https://rustup.rs) stable (see `rust-toolchain.toml`)
- MSVC C++ build tools (“Desktop development with C++”)
- WebView2 (included on modern Windows 10/11)
- No Node.js required to run the UI (static files under `crates/minter-desktop/ui`)

Optional on other OSes: you can develop and test **`minter-core`** on Linux/macOS; the **desktop GUI** is built and supported for Windows.

## Build & test

From the repo root:

```powershell
# Core library tests (also what CI runs)
cargo test -p minter-core --lib
cargo clippy -p minter-core --all-targets -- -D warnings
cargo fmt --all -- --check

# Desktop app (Windows)
cargo run -p minter-desktop --release
```

Package a local ship folder (exe only; secrets never copied):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
```

## Pull requests

1. Fork (or branch from `main` if you have write access).
2. Create a branch: `fix/…`, `feat/…`, or `docs/…`.
3. Make sure `cargo fmt`, `clippy -D warnings`, and `cargo test -p minter-core --lib` pass.
4. Open a PR against `main` with a short description of **what** and **why**.
5. Link related issues when applicable.

### What we look for

- Clear commit messages (conventional style welcome: `fix:`, `feat:`, `docs:`, `security:`)
- No secrets in the diff or in committed sample data
- Tests for non-trivial core logic when practical
- Docs updated if behavior or UX changed

## Security issues

Do **not** open a public issue for vulnerabilities that could leak vault keys or forge mint transactions. See [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions are licensed under the same terms as the project: **MIT OR Apache-2.0**.
