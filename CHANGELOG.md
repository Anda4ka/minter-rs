# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Public-launch hygiene: CONTRIBUTING, Code of Conduct, issue/PR templates
- Release workflow (tag-driven Windows build artifacts)
- Operator guide (EN) and packaging script aligned with post-cleanup layout

### Changed

- Cargo package metadata (license, repository, authors) on core and desktop
- SECURITY.md with concrete reporting channels
- README polished for public announcement

## [0.1.0] - 2026-07-24

### Added

- **minter-core** — shared mint / vault / RPC engine (OpenSea SeaDrop + raw sniper)
- **minter-desktop** — Tauri 2 Windows GUI (tasks, Mission Control, wallets, proxies, raw mint)
- Encrypted vault (AES-256-GCM, PBKDF2 600k, Zeroizing)
- LIVE confirm gate, idle lock, dry-run defaults
- Multi-wallet sticky proxies (OpenSea auth path)
- Private Alchemy multi-chain RPC (user key); hedged reads / fan-out
- Results export (JSON/CSV), run history, full mint logs
- Dual license: MIT OR Apache-2.0
- CI: rustfmt, clippy `-D warnings`, `minter-core` tests

### Security

- Tauri CSP (non-null)
- Session / vault Debug redaction
- Wave A–D hardening (LIVE gate, fee caps, zero-address rejects, OpenSea value checks, etc.)

[Unreleased]: https://github.com/Anda4ka/minter-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Anda4ka/minter-rs/releases/tag/v0.1.0
