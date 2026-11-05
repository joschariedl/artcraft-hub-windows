# ArtCraft Hub (Windows)

A native installer and updater for the Craft Apps on Windows –
a single self-contained executable, no installer, no Electron, no web view.

ArtCraft Hub downloads the official Windows packages straight from the GitHub
releases, verifies them against the release's `SHA256SUMS.txt` **and** VirusTotal,
installs them per-user into `%LOCALAPPDATA%\ArtCraft`, and creates a Start Menu
entry with a high-resolution icon.

It ships a single self-contained executable:

- **`artcraft-hub`** – a native GUI (Rust + egui/eframe)

All user-facing text is in English.

---

![ArtCraft Hub](assets/screenshot.png)

## Build from source

Source archives are published on the
[releases page](https://github.com/joschariedl/artcraft-hub-windows/releases).
The build requires Rust 1.85+.

```text
git clone https://github.com/joschariedl/artcraft-hub-windows.git
cd artcraft-hub-windows
cargo build --workspace --release
```

The resulting `artcraft-hub.exe` runs directly – no installation required.

---

## The apps

| App | Area | Repository |
| --- | --- | --- |
| PhotoCraft | Image editing | `storytold/photocraft` |
| VectorCraft | Vector illustration | `storytold/vectorcraft` |
| FilmCraft | Video editing, colour and sound | `storytold/filmcraft` |
| LightCraft | Photo library and RAW development | `storytold/lightcraft` |
| PdfCraft | Read, organise and protect PDFs | `storytold/pdfcraft` |
| EffectCraft | Motion graphics and VFX | `storytold/effectcraft` |
| DesignCraft | Page layout and publishing | `storytold/designcraft` |
| FigCraft | Interface design and prototyping | `storytold/figcraft` |
| TextureCraft | 3D texture painting | `storytold/texturecraft` |
| MaterialCraft | Procedural materials | `storytold/materialcraft` |
| WordCraft | Word processing and document design | `storytold/wordcraft` |
| SoundCraft | Record, edit and mix audio | `storytold/soundcraft` |
| DeckCraft | Presentations and slideshows | `storytold/deckcraft` |
| CADCraft | CAD and technical drawing | `storytold/cadcraft` |
| GridCraft | Spreadsheets and data analysis | `storytold/gridcraft` |

All 15 supported apps have public releases. An app shows as "no release" only when GitHub's API limit
(60 requests/hour without a token) is reached – the release feed does not include every app,
so there is no fallback in that case. A GitHub token (settings) fixes it.

---

## Security model

Every install and update passes three stages. If any one fails, **nothing is installed**.

1. **SHA-256.** The archive hash from `SHA256SUMS.txt` is checked against the file's own
   SHA-256 and against the digest published by the GitHub API for that asset. A mismatch aborts.
2. **VirusTotal.** The file's SHA-256 is looked up (never uploaded) via the VirusTotal public
   API. Clean, unknown or missing verdicts are handled according to the settings; a *malicious*
   verdict blocks the install. Results are cached for 7 days.
3. **GPG (optional).** If a `.sig`/`.asc` asset exists, it is verified with `gpg`; if `gpg`
   is missing, the check is skipped with a note.

### Transactional installs and rollback

Installs go into a staging directory and are atomically moved into place. If activation fails
(desktop file, shortcut, icons), the previous version is restored. Each app keeps its two most
recent versions, so `artcraftctl rollback <app>` works offline.

### Optional trust anchors

- **Signed manifest:** `SHA256SUMS.txt` may carry a detached signature (`SHA256SUMS.txt.sig` or `.asc`),
  verified against a configurable GPG keyring and trusted fingerprints.
- **GitHub attestations:** Sigstore provenance (`gh attestation verify`) is checked when present.

### Fail-closed

No VirusTotal key means no install. Missing `SHA256SUMS.txt` is rejected unless explicitly allowed.
A hash conflict stops everything.

### What the hub does *not* do

- No root / no UAC elevation. Everything lands in the user profile.
- No telemetry, no background service.
- No auto-update of the hub itself (download a new release manually).

---

## Requirements

- Windows 10 or 11 (x86_64)
- A free [VirusTotal API key](https://www.virustotal.com/gui/my-apikey)
- `artcraft-hub.exe` on `PATH` (optional)

The GUI needs no runtime dependencies beyond what ships with Windows.

---

## Usage

### GUI

```text
artcraft-hub.exe
```

Or double-click it. The UI shows all 15 supported apps with icon, installed
version and the latest available version.

- **Install** – downloads, verifies (SHA-256 + VirusTotal) and installs the current version.
- **Update** – appears once a newer version is available; per app or all at once. An upward arrow
  on the right edge of the row marks an app with a pending update.
- **Remove** – removes the program and its Start Menu entry.
- **Refresh** – forces a network query instead of the 30-minute cache.
- **Settings** – VirusTotal key, GitHub token, check interval, icon options, language.

- **Filters & search** – filter by All/Updates/Installed/Not installed, search box, keyboard shortcuts (F5 refresh, Ctrl+F search, Ctrl+U updates, 1–4 filters, Esc close).
During a download the GUI shows progress and the current verification stage. Errors are
explained in plain text (missing key, hash conflict, VirusTotal finding, no release, …).

---

## Data paths

| Content | Path |
| --- | --- |
| Configuration | `%APPDATA%\artcraft-hub\config.toml` |
| Status / installed versions | `%LOCALAPPDATA%\ArtCraft\…\state.json` |
| VirusTotal cache | `%LOCALAPPDATA%\ArtCraft\…\virustotal-cache.json` |
| Downloads, icons | `%LOCALAPPDATA%\ArtCraft\…\cache\` |
| Installed apps | `%LOCALAPPDATA%\ArtCraft\<id>\<version>\` |
| Start Menu entries | `%APPDATA%\Microsoft\Windows\Start Menu\Programs\ArtCraft\` |

Several versions of an app can live in parallel under `%LOCALAPPDATA%\ArtCraft\<id>\`; the
active one is launched from its version directory. Uninstalling removes all versions of the app.

---

## Configuration

`%APPDATA%\artcraft-hub\config.toml` (created on first save):

| Key | Default | Meaning |
| --- | --- | --- |
| `github_token` | `""` | Optional; raises the GitHub limit from 60 to 5,000 requests/hour |
| `virustotal_api_key` | `""` | Required for installs (or `VIRUSTOTAL_API_KEY`) |
| `metadata_cache_minutes` | `30` | Lifetime of release metadata |
| `check_interval_minutes` | `60` | Recommended check interval |
| `require_sha256sums` | `true` | Reject releases without `SHA256SUMS.txt` |
| `install_1024_icons` | `true` | Also install the 1024 px PNG from the app repository |
| `language` | `en` | UI language |
| `keep_versions` | `2` | Versions kept per app (active + rollback targets) |
| `parallel_checks` | `4` | Parallel release lookups during status checks |
| `network_retries` | `3` | Network attempts including the first one |
| `verify_attestations` | `true` | Verify GitHub/Sigstore attestations when present |
| `require_attestations` | `false` | Fail when a release has no attestation |
| `verify_signed_manifest` | `true` | Verify a detached signature over `SHA256SUMS.txt` |
| `require_signed_manifest` | `false` | Fail when `SHA256SUMS.txt` is not signed |
| `require_gpg_signature` | `false` | Fail when an archive has no GPG signature |
| `gpg_keyring` | `""` | Dedicated GPG keyring (empty = default) |
| `gpg_trusted_fingerprints` | `[]` | Trusted signer fingerprints (empty = any valid) |
| `vt.min_interval_seconds` | `16` | Minimum spacing between VirusTotal requests |
| `vt.cache_days` | `7` | Lifetime of a VirusTotal result |
| `vt.poll_seconds` | `20` | Wait between status polls of a URL scan |
| `vt.max_polls` | `15` | Maximum number of status polls |
| `vt.allow_unknown` | `false` | Allow unclear results (**not recommended**) |

The key, the GitHub token and `allow_unknown` can be changed via `artcraftctl config`; everything
else via editor or in the GUI.

---

## Troubleshooting

**"VirusTotal key missing" / installs locked.**
Configure a key: `artcraftctl config --set-vt-key <KEY>`, then `artcraftctl doctor`.

**"VirusTotal quota exhausted" or HTTP 429.**
The public API allows 4 requests/minute and 500/day. The hub throttles itself to one request
every 16 seconds and caches results for 7 days. When testing frequently, wait or use your own key.

**"Hash mismatch" / hash conflict.**
The file differs from `SHA256SUMS.txt` or from the GitHub asset digest. This can be a broken
download – retry. If it persists, do not install and report the release in the app repository.

**"No release" for FigCraft (or other apps).**
Usually a reached GitHub API limit (60 requests/hour without a token). The release feed does
not contain FigCraft, so there is no fallback. Solution: `artcraftctl config --set-github-token <TOKEN>`
or wait. With a token (5,000 requests/hour) this no longer happens.

**GitHub API limit (HTTP 403).**
Without a token GitHub allows 60 requests/hour. The feed fallback kicks in automatically;
optionally set `artcraftctl config --set-github-token <TOKEN>`.

**App does not appear in the Start Menu.**
Check that the shortcut exists under
`%APPDATA%\Microsoft\Windows\Start Menu\Programs\ArtCraft` and re-run the install.
Make sure the version directory under `%LOCALAPPDATA%\ArtCraft` is intact.

**GUI does not start.**
The GUI uses eframe/glow; on systems without suitable GL drivers try running with
`LIBGL_ALWAYS_SOFTWARE=1` (if supported) or update your graphics drivers.

---

## Development

```text
cargo build --workspace              # debug
cargo build --workspace --release    # release
cargo test --workspace               # tests (44 in artcraft-core)
cargo clippy --workspace --all-targets
```

Layout:

```
crates/artcraft-core/   catalogue, GitHub, VirusTotal, download, installation, status
crates/artcraft-hub/    GUI (eframe/egui)
assets/                 Hub icon and embedded app icons
.github/workflows/      CI (Windows + Linux builds)
```

The core logic is free of UI dependencies and is shared by GUI and CLI through
`ops::{check_all, install, upgrade_all, uninstall, doctor}`.

The Windows install backend lives in `crates/artcraft-core/src/install/windows.rs`.

---

## License

MIT – see `LICENSE`. Copyright (c) 2026 Joscha Riedl.

ArtCraft itself is a product of Storytold; this project is not
officially affiliated with it and installs only the official release packages.
