# ArtCraft Hub – Omarchy bar plugin

Small bar indicator for the 15 Craft Apps supported by ArtCraft Hub. A paintbrush icon in the top right
turns coloured as soon as an update is available; a click opens the overview.

## What it does

- reads `artcraftctl status --json` (cached by default, so the bar does not hit
  the network on every timer tick)
- shows installed version → new version per app
- starts `artcraftctl upgrade …` in a floating terminal. The SHA-256 comparison
  and VirusTotal check run visibly there; without a valid VirusTotal key nothing
  is installed.
- warns when no VirusTotal key is configured and offers a jump into the hub

The query runs every 30 minutes and when the popup opens. The "Check now" button
(or the `r` key) forces a fresh GitHub query.

## Dependencies

- `artcraftctl` in `~/.local/bin` (installed by `scripts/install.sh`)
- `artcraft-hub` in `~/.local/bin` for the "Hub" button

## Installation

```bash
./scripts/install.sh
```

The script copies the plugin to
`~/.config/omarchy/plugins/derfleischmann.artcraft` and activates it in the
right bar section. Manually:

```bash
omarchy plugin validate ~/.config/omarchy/plugins/derfleischmann.artcraft
omarchy-shell shell rescanPlugins
omarchy plugin enable derfleischmann.artcraft right
```

## Usage

| Action | Effect |
|---|---|
| Left click | Open/close popup |
| Middle click | Open popup |
| `r` in popup | Re-check |
| `o` in popup | Open hub |
| Enter in popup | Start all updates (or re-check) |
| Esc | Close |

IPC:

```bash
omarchy-shell derfleischmann.artcraft toggle
omarchy-shell derfleischmann.artcraft refresh
```