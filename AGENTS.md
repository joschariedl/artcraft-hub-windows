# AGENTS.md – ArtCraft Hub (Windows)

This file documents the project for AI agents (and humans) working on the codebase. It describes
the architecture, data flow, key decisions, file layout and conventions so changes stay consistent.

## Project overview

ArtCraft Hub (Windows) is a native installer/updater for the 15 Craft Apps supported by
ArtCraft Hub (https://getartcraft.com/apps) on Windows 10/11 (x86_64). It downloads the official
Windows portable-zip releases from GitHub, verifies them (SHA-256 + VirusTotal + optional
GPG), installs them per-user into `%LOCALAPPDATA%\ArtCraft`, creates Start Menu shortcuts, and
provides a native GUI and CLI.

- Language: all user-facing text is **English**.
- License: MIT.
- Author: Joscha Riedl (online handle: derfleischmann).
- Repository: https://github.com/joschariedl/artcraft-hub-windows (public).

## Workspace layout

```
artcraft-hub-windows/
├── Cargo.toml                  # workspace root
├── crates/
│   ├── artcraft-core/          # shared library (no UI deps)
│   └── artcraft-hub/           # GUI (eframe/egui)
├── assets/
│   ├── artcraft-hub.svg        # hub icon
│   └── icons/                  # embedded app icons (PNG, 256x256, padding-free)
├── .github/workflows/
│   └── build.yml               # Windows + Linux CI
├── README.md
├── AGENTS.md
└── LICENSE
```

## Crates

### artcraft-core (`crates/artcraft-core`)

Shared library. No UI dependencies. Used by both GUI and CLI.

Key modules:
- `catalog.rs` – `AppDef`, `SUITE: [AppDef; 15]`, `by_id()`, `release_arch()`, `icon_paths()`.
  - 15 apps in website order. pdfcraft has stem `printcraft`.
- `config.rs` – `Config` (TOML, 0600 on Unix). Fields: github_token, virustotal_api_key, metadata_cache_minutes,
  check_interval_minutes, require_sha256sums, install_1024_icons, keep_versions, parallel_checks,
  network_retries, verify_attestations, require_attestations, verify_signed_manifest,
  require_signed_manifest, require_gpg_signature, gpg_keyring, gpg_trusted_fingerprints,
  follow_omarchy_theme, vt (VtConfig), language (default "en").
- `paths.rs` – `Paths::from_env()`. On Windows: config `%APPDATA%\artcraft-hub`, install root
  `%LOCALAPPDATA%\ArtCraft`, state/cache under there, Start Menu shortcuts under
  `%APPDATA%\Microsoft\Windows\Start Menu\Programs\ArtCraft`. `ensure()`.
- `github.rs` – release discovery. `latest()`, `resolve()`, `resolve_conditional()`, `resolve_tag()`,
  `plan_from_release()`, `select_asset()` (Windows: portable zip / msi; Unix: tar.gz).
- `vt.rs` – VirusTotal client, URL-scan flow, caching.
- `download.rs` – resumable downloads with SHA-256 verification.
- `install/` – `mod.rs` routes to `unix.rs` or `windows.rs` based on target OS.
  - `windows.rs`: portable-zip extraction via PowerShell, Start Menu `.lnk` shortcuts, pruning.
- `ops.rs` – high-level ops shared by GUI and CLI.
- `state.rs` – installed versions, cached releases, history, pins, skipped.
- `theme.rs` – Theme loading (Omarchy palette on Linux; built-in fallback elsewhere).
- `lock.rs` – cross-process lock (flock on Unix; no-op on Windows).
- `trust.rs` – attestations, manifest signatures, GPG.

### artcraft-hub (`crates/artcraft-hub`)

Native GUI. eframe 0.37 + egui 0.37, glow backend, Windows + Linux.

- `main.rs` – `HubApp`. Views: Apps, Settings. Panels: top, bottom, left sidebar, central.
- Embedded icons via `include_bytes!`.
- `--version`/`--help` handled in main().

## Security model

- Download only from `https://github.com/<repo>/releases/download/<tag>/<name>`.
- SHA-256 checked against `SHA256SUMS.txt`; `SHA256SUMS.txt` checked against GitHub asset digest.
- VirusTotal: file report → URL scan → content hash comparison. Fail-closed. No file upload.
- Optional GPG verification.
- No elevation; installs only in the user profile.

## Key decisions

- **eframe 0.37 API**: `App::ui(&mut self, ui, frame)`. Panels via `egui::Panel::{top,bottom,left}`.
- **ureq 3.5** (blocking) instead of Tokio. HTTP in background threads.
- **No Electron/webview**: native Rust.
- **Icons**: embedded WebP→PNG (256×256, padding-free) via `include_bytes!`.
- **Cross-platform install**: `install/` module routes by `target_os`.
- **Windows specifics**: portable-zip assets, PowerShell for extraction/shortcuts, no Omarchy integration.

## Conventions

- All user-facing text in English.
- Error messages via `thiserror` in `error.rs`, all English.
- Rust edition 2021, rust-version 1.85. Clippy clean (0 warnings).
- Tests: 44 in artcraft-core. Run `cargo test --workspace`.

## Build & CI

```bash
cargo build --workspace --release
cargo test --workspace
cargo clippy --workspace --all-targets
```

CI (`.github/workflows/build.yml`) builds on `windows-latest` and `ubuntu-latest`.

## Common pitfalls

- Windows install backend uses PowerShell (`Expand-Archive`, `WScript.Shell`); ensure PowerShell
  is available (default on Windows 10/11).
- The `image` crate features are workspace-defined; keep `webp` available for embedded icons.
- eframe Wayland/X11 features are Linux-only; do not enable them on Windows.
- No Omarchy bar plugin on Windows.

## Recent changes (summary)

- Initial Windows port from artcraft-hub: platform guards, Windows paths, asset selection,
  PowerShell-based install backend, CI workflow.
- Embedded icons for 15 apps.
- Windows standalone `.exe` published as v0.1.0.
- Releases are source-only (GitHub's auto-generated source archives); build with
  `cargo build --workspace --release`.
