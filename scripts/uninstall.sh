#!/usr/bin/env bash
# Removes ArtCraft Hub, artcraftctl and the Omarchy bar plugin.
#
# Installed Craft Apps stay where they are unless --apps is given.
set -euo pipefail

readonly PLUGIN_ID="derfleischmann.artcraft"
readonly CONFIG_DIR="${HOME}/.config/artcraft-hub"
readonly STATE_DIR="${HOME}/.local/state/artcraft-hub"
readonly CACHE_DIR="${HOME}/.cache/artcraft-hub"

purge=0
remove_apps=0

usage() {
  cat <<'EOF'
Usage: scripts/uninstall.sh [options]

  --apps    Also remove all installed ArtCraft apps
  --purge   Also delete configuration, status and cache
            (includes the stored VirusTotal key)
  -h, --help
EOF
}

for arg in "$@"; do
  case "$arg" in
    --apps) remove_apps=1 ;;
    --purge) purge=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
  esac
done

ctl="${HOME}/.local/bin/artcraftctl"

if (( remove_apps )); then
  if [[ -x "$ctl" ]]; then
    echo "==> Removing installed ArtCraft apps"
    for app in photocraft vectorcraft filmcraft lightcraft pdfcraft effectcraft \
               designcraft figcraft wordcraft soundcraft deckcraft cadcraft gridcraft; do
      "$ctl" uninstall "$app" --yes || true
    done
  else
    echo "artcraftctl missing – apps are not removed." >&2
  fi
fi

echo "==> Removing Omarchy plugin"
if command -v omarchy >/dev/null 2>&1; then
  omarchy plugin remove "$PLUGIN_ID" --yes >/dev/null 2>&1 || true
fi
rm -rf "${HOME}/.config/omarchy/plugins/${PLUGIN_ID}"
omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true

echo "==> Removing programs, icon and desktop file"
rm -f "${HOME}/.local/bin/artcraft-hub" "${HOME}/.local/bin/artcraftctl"
rm -f "${HOME}/.local/share/applications/artcraft-hub.desktop"
rm -f "${HOME}/.local/share/icons/hicolor/scalable/apps/artcraft-hub.svg"
update-desktop-database "${HOME}/.local/share/applications" >/dev/null 2>&1 || true
gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true

if (( purge )); then
  echo "==> Deleting configuration, status and cache"
  rm -rf "$CONFIG_DIR" "$STATE_DIR" "$CACHE_DIR"
fi

printf '\nDone.\n'
(( remove_apps )) || echo "Note: installed apps under ~/.local/opt/artcraft are kept."