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
# The launcher then works whether or not ~/.local/bin is on PATH. Only the
# binary name is replaced, never the whole line: the `%F` after it is what
# hands rulogman the paths a file manager asked it to open.
install -d "$prefix/applications"
sed "s|^Exec=rulogman|Exec=$bindir/rulogman|" "$here/com.aihouse.rulogman.desktop" \
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

# What rulogman needs from the system, checked here rather than left to fail at
# the first launch. It is linked against libxcb and both halves of xkbcommon,
# so the loader will not start it while any of them is missing; it draws
# through Vulkan or, without it, EGL, loaded at run time; and in a Wayland
# session it loads the Wayland client library. Nothing is installed from here,
# since that would need root: what is missing is listed together with the
# command that installs it. The glibc floor is the one the release build is
# checked against in .github/workflows/release.yml.
glibc_floor=2.35

ldconfig_bin="$(command -v ldconfig 2>/dev/null || true)"
for candidate in /sbin/ldconfig /usr/sbin/ldconfig; do
    if [ -z "$ldconfig_bin" ] && [ -x "$candidate" ]; then
        ldconfig_bin="$candidate"
    fi
done

has_lib() {
    if [ -n "$ldconfig_bin" ] &&
        "$ldconfig_bin" -p 2>/dev/null | grep -q "^[[:space:]]*$1 (libc6,x86-64"; then
        return 0
    fi
    for dir in /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu /usr/lib64 /lib64 /usr/lib /lib; do
        if [ -e "$dir/$1" ]; then
            return 0
        fi
    done
    return 1
}

if command -v apt-get >/dev/null 2>&1; then
    manager=apt
elif command -v dnf >/dev/null 2>&1; then
    manager=dnf
elif command -v pacman >/dev/null 2>&1; then
    manager=pacman
elif command -v zypper >/dev/null 2>&1; then
    manager=zypper
else
    manager=
fi

# The package that provides each library under each package manager. EGL is
# asked for together with Mesa's implementation of it: the loader alone draws
# nothing.
package_for() {
    case "$manager:$1" in
        apt:libxcb.so.1) echo libxcb1 ;;
        apt:libxkbcommon.so.0) echo libxkbcommon0 ;;
        apt:libxkbcommon-x11.so.0) echo libxkbcommon-x11-0 ;;
        apt:libEGL.so.1) echo libegl1 libegl-mesa0 ;;
        apt:libwayland-client.so.0) echo libwayland-client0 ;;
        dnf:libxcb.so.1) echo libxcb ;;
        dnf:libxkbcommon.so.0) echo libxkbcommon ;;
        dnf:libxkbcommon-x11.so.0) echo libxkbcommon-x11 ;;
        dnf:libEGL.so.1) echo mesa-libEGL ;;
        dnf:libwayland-client.so.0) echo libwayland-client ;;
        pacman:libxcb.so.1) echo libxcb ;;
        pacman:libxkbcommon.so.0) echo libxkbcommon ;;
        pacman:libxkbcommon-x11.so.0) echo libxkbcommon-x11 ;;
        pacman:libEGL.so.1) echo mesa ;;
        pacman:libwayland-client.so.0) echo wayland ;;
        zypper:libxcb.so.1) echo libxcb1 ;;
        zypper:libxkbcommon.so.0) echo libxkbcommon0 ;;
        zypper:libxkbcommon-x11.so.0) echo libxkbcommon-x11-0 ;;
        zypper:libEGL.so.1) echo Mesa-libEGL1 ;;
        zypper:libwayland-client.so.0) echo libwayland-client0 ;;
    esac
}

missing=
for lib in libxcb.so.1 libxkbcommon.so.0 libxkbcommon-x11.so.0; do
    has_lib "$lib" || missing="$missing $lib"
done
if ! has_lib libvulkan.so.1 && ! has_lib libEGL.so.1; then
    missing="$missing libEGL.so.1"
fi
if [ -n "${WAYLAND_DISPLAY:-}" ] && ! has_lib libwayland-client.so.0; then
    missing="$missing libwayland-client.so.0"
fi

glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null | sed 's/^glibc //' || true)"
old_glibc=
if [ -z "$glibc" ]; then
    old_glibc="no glibc was found"
elif [ "$(printf '%s\n' "$glibc_floor" "$glibc" | sort -V | head -n 1)" != "$glibc_floor" ]; then
    old_glibc="this system has glibc $glibc"
fi

if [ -z "$missing" ] && [ -z "$old_glibc" ]; then
    echo "  runtime check: glibc $glibc and every library rulogman needs are present"
fi

if [ -n "$old_glibc" ]; then
    echo
    echo "warning: rulogman needs glibc $glibc_floor or newer, and $old_glibc."
    echo "  It will not start here; it runs on Ubuntu 22.04, Debian 12 and later, or a"
    echo "  distribution built on them."
fi

if [ -n "$missing" ]; then
    echo
    echo "warning: rulogman needs libraries this system does not have:"
    for lib in $missing; do
        echo "  $lib"
    done
    packages=
    for lib in $missing; do
        packages="$packages $(package_for "$lib")"
    done
    case "$manager" in
        apt) echo "install them with:"; echo "  sudo apt install$packages" ;;
        dnf) echo "install them with:"; echo "  sudo dnf install$packages" ;;
        pacman) echo "install them with:"; echo "  sudo pacman -S$packages" ;;
        zypper) echo "install them with:"; echo "  sudo zypper install$packages" ;;
        *) echo "install the packages that provide them with your distribution's package manager." ;;
    esac
fi
