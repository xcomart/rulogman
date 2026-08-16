#!/bin/sh
# Installs rulogman for the current user: binary, desktop entry and icons.
# Run from the unpacked release directory. No root required.
set -eu

prefix="${XDG_DATA_HOME:-$HOME/.local/share}"
bindir="$HOME/.local/bin"

here="$(cd "$(dirname "$0")" && pwd)"

install -Dm755 "$here/rulogman" "$bindir/rulogman"

# Exec= in a desktop entry takes no shell variables, so $HOME cannot be
# written there; put the absolute binary path in at install time instead.
# The launcher then works whether or not ~/.local/bin is on PATH.
install -d "$prefix/applications"
sed "s|^Exec=.*|Exec=$bindir/rulogman|" "$here/com.aihouse.rulogman.desktop" \
    > "$prefix/applications/com.aihouse.rulogman.desktop"
chmod 644 "$prefix/applications/com.aihouse.rulogman.desktop"
install -Dm644 "$here/icons/rulogman-128.png" "$prefix/icons/hicolor/128x128/apps/rulogman.png"
install -Dm644 "$here/icons/rulogman-256.png" "$prefix/icons/hicolor/256x256/apps/rulogman.png"
install -Dm644 "$here/icons/rulogman.svg" "$prefix/icons/hicolor/scalable/apps/rulogman.svg"

# Refresh caches when the tools are around; harmless to skip.
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$prefix/applications" || true
command -v gtk-update-icon-cache  >/dev/null 2>&1 && gtk-update-icon-cache -q "$prefix/icons/hicolor" || true

echo "installed rulogman to $bindir"
echo "  the desktop launcher works as is; add $bindir to PATH to run it from a shell"
echo "  runtime dependency: libxkbcommon-x11-0 (Debian/Ubuntu) / libxkbcommon-x11 (Fedora, Arch)"
