#!/usr/bin/env bash
# Builds and installs ArtCraft Hub, artcraftctl and the Omarchy bar plugin.
#
# Everything lands in the user's home – no root, no system directories.
set -euo pipefail

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly PLUGIN_ID="derfleischmann.artcraft"
readonly BIN_DIR="${HOME}/.local/bin"
readonly ICON_DIR="${HOME}/.local/share/icons/hicolor/scalable/apps"
readonly APPLICATIONS_DIR="${HOME}/.local/share/applications"
readonly PLUGIN_DIR="${HOME}/.config/omarchy/plugins/${PLUGIN_ID}"

profile="release"
enable_plugin=1
install_plugin=1

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [options]

  --debug         Debug build instead of release
  --no-plugin     Do not install the Omarchy bar plugin
  --no-enable     Install the plugin but do not add it to the bar
  -h, --help      This help

After installation: artcraftctl config --set-vt-key <KEY>
EOF
}

for arg in "$@"; do
  case "$arg" in
    --debug) profile="debug" ;;
    --no-plugin) install_plugin=0; enable_plugin=0 ;;
    --no-enable) enable_plugin=0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
  esac
done

cargo_bin="${CARGO:-}"
if [[ -z "$cargo_bin" ]]; then
  if [[ -x "${HOME}/.cargo/bin/cargo" ]]; then
    cargo_bin="${HOME}/.cargo/bin/cargo"
  elif command -v cargo >/dev/null 2>&1; then
    cargo_bin="$(command -v cargo)"
  else
    echo "cargo not found. Install Rust: https://rustup.rs" >&2
    exit 1
  fi
fi

printf '\n==> Building ArtCraft Hub (%s)\n' "$profile"
build_args=(build --workspace)
[[ "$profile" == "release" ]] && build_args+=(--release)
(cd "$REPO_ROOT" && "$cargo_bin" "${build_args[@]}")

readonly TARGET_DIR="${REPO_ROOT}/target/${profile}"

printf '\n==> Installing programs to %s\n' "$BIN_DIR"
install -d -m 0755 "$BIN_DIR"
install -m 0755 "${TARGET_DIR}/artcraftctl" "${BIN_DIR}/artcraftctl"
install -m 0755 "${TARGET_DIR}/artcraft-hub" "${BIN_DIR}/artcraft-hub"

printf '==> Installing shell completions\n'
for shell in bash zsh fish; do
  case "$shell" in
    bash) dir="${HOME}/.local/share/bash-completion/completions"; file="artcraftctl" ;;
    zsh)  dir="${HOME}/.local/share/zsh/site-functions"; file="_artcraftctl" ;;
    fish) dir="${HOME}/.config/fish/completions"; file="artcraftctl.fish" ;;
  esac
  install -d -m 0755 "$dir"
  "${BIN_DIR}/artcraftctl" completions "$shell" > "${dir}/${file}" 2>/dev/null || true
done

printf '==> Installing icon and desktop file\n'
install -d -m 0755 "$ICON_DIR" "$APPLICATIONS_DIR"
install -m 0644 "${REPO_ROOT}/assets/artcraft-hub.svg" "${ICON_DIR}/artcraft-hub.svg"

cat >"${APPLICATIONS_DIR}/artcraft-hub.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=ArtCraft Hub
GenericName=Manage ArtCraft apps
Comment=Install and update the 13 ArtCraft apps
Exec=${BIN_DIR}/artcraft-hub
TryExec=${BIN_DIR}/artcraft-hub
Icon=artcraft-hub
Terminal=false
Categories=Utility;
Keywords=ArtCraft;Design;Photo;Video;Audio;Update;Installer;
StartupNotify=true
StartupWMClass=artcraft-hub
EOF
chmod 0644 "${APPLICATIONS_DIR}/artcraft-hub.desktop"
update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true

if (( install_plugin )); then
  printf '\n==> Installing Omarchy plugin to %s\n' "$PLUGIN_DIR"
  if ! command -v omarchy-shell >/dev/null 2>&1; then
    echo "Note: omarchy-shell not found – the plugin is copied but not activated." >&2
    enable_plugin=0
  fi

  rm -rf "$PLUGIN_DIR"
  install -d -m 0755 "$PLUGIN_DIR"
  install -m 0644 "${REPO_ROOT}/plugins/${PLUGIN_ID}/manifest.json" "$PLUGIN_DIR/manifest.json"
  install -m 0644 "${REPO_ROOT}/plugins/${PLUGIN_ID}/BarWidget.qml" "$PLUGIN_DIR/BarWidget.qml"
  install -m 0644 "${REPO_ROOT}/plugins/${PLUGIN_ID}/ArtcraftPanel.qml" "$PLUGIN_DIR/ArtcraftPanel.qml"
  install -m 0644 "${REPO_ROOT}/plugins/${PLUGIN_ID}/README.md" "$PLUGIN_DIR/README.md" 2>/dev/null || true

  if command -v omarchy >/dev/null 2>&1; then
    if omarchy plugin validate "$PLUGIN_DIR" >/dev/null 2>&1; then
      echo "    Manifest valid."
    else
      echo "Warning: omarchy plugin validate reports issues for $PLUGIN_DIR" >&2
    fi
  fi

  if (( enable_plugin )); then
    omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
    plugin_enabled=$(omarchy plugin list --json 2>/dev/null | python3 -c '
import json, sys
target = "derfleischmann.artcraft"
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(2)
plugins = data if isinstance(data, list) else data.get("plugins", [])
for plugin in plugins:
    if plugin.get("id") == target and plugin.get("enabled") is True:
        print("yes")
        break
' 2>/dev/null || true)
    if [[ "$plugin_enabled" == "yes" ]]; then
      echo "    Plugin is already active."
    elif omarchy plugin enable "$PLUGIN_ID" right >/dev/null 2>&1; then
      echo "    Plugin activated in the right bar section."
    else
      echo "Note: the plugin could not be activated automatically." >&2
      echo "         Manually: omarchy plugin enable $PLUGIN_ID right" >&2
    fi
  fi
fi

printf '\n==> Done.\n\n'
"${BIN_DIR}/artcraftctl" doctor || true
cat <<EOF

Next steps:
  1. Configure a VirusTotal API key (required for every install):
       artcraftctl config --set-vt-key <KEY>     # Key: https://www.virustotal.com/gui/my-apikey
     or in the Hub under "Settings".
  2. Start the hub:
       artcraft-hub
  3. Status in the bar / on the command line:
       artcraftctl status
EOF