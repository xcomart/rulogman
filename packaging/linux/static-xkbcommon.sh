#!/usr/bin/env bash
# Builds the static xkbcommon the Linux release binary links in, so that
# users need neither libxkbcommon, libxkbcommon-x11 nor libxcb-xkb installed:
#
#   packaging/linux/static-xkbcommon.sh <dir>
#
# leaves <dir>/libxkbcommon.a, and <dir>/libxkbcommon-x11.a as a link to the
# same archive. The release build puts <dir> on the linker's search path
# (`RUSTFLAGS=-L native=<dir>`), so the `-lxkbcommon` and `-lxkbcommon-x11`
# the xkbcommon crate asks for find these archives. The build host must have
# no libxkbcommon*.so development links of its own, or the linker would take
# those instead; release.yml then checks what the binary still loads.
#
# Why all three, and in one archive:
# - libxkbcommon-x11 builds `struct xkb_keymap` itself from libxkbcommon's
#   private headers and hands it to libxkbcommon, which is why distributions
#   pin the two to exactly the same version. The X11 half linked in on its
#   own, next to whatever libxkbcommon.so a user has, would mix two layouts
#   of that struct.
# - libxkbcommon-x11 needs libxcb-xkb, which desktops do not install by
#   default either. libxcb itself stays dynamic: every X client has it.
# - With everything in one archive there is no link order to get right: the
#   linker takes what it can from an archive before it moves on, and the
#   xkbcommon crate names its two libraries in no promised order.
#
# The data paths are compiled in, as in every distribution's own build, and
# set to the ones Debian, Ubuntu, Fedora, Arch and openSUSE all use. Only the
# compose tables are read at run time (dead keys and the Compose key); the
# keymap itself comes from the X server or the Wayland compositor. On a
# system that keeps the tables elsewhere, XLOCALEDIR still points at them.
#
# libxkbcommon 1.13 wants meson 1.4 or newer and Debian 12 has 1.0, so a
# pinned meson release runs from its unpacked source instead of being
# installed. Both downloads are checked against their SHA256s; to upgrade
# either, change its version and SHA256 together.
#
# Needs a C compiler, ar, bison, ninja, python3, curl, pkg-config and
# libxcb-xkb's development files with their static archive, as Debian's
# libxcb-xkb-dev ships them.
set -euo pipefail

XKBCOMMON_VERSION=1.13.2
XKBCOMMON_SHA256=acc4d5f7c3cbba5f9f8d08d8bdbeede84ecede46792f47929aa9321873385528
MESON_VERSION=1.12.1
MESON_SHA256=ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743

out=${1:?usage: $0 <output dir>}
mkdir -p "$out"
out=$(cd "$out" && pwd)

xcb_xkb="$(pkg-config --variable=libdir xcb-xkb)/libxcb-xkb.a"
if [ ! -f "$xcb_xkb" ]; then
    echo "error: no static libxcb-xkb ($xcb_xkb); install libxcb-xkb's development package" >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

curl --proto '=https' --tlsv1.2 -sSfL -o xkbcommon.tar.gz \
    "https://github.com/xkbcommon/libxkbcommon/archive/refs/tags/xkbcommon-$XKBCOMMON_VERSION.tar.gz"
curl --proto '=https' --tlsv1.2 -sSfL -o meson.tar.gz \
    "https://github.com/mesonbuild/meson/releases/download/$MESON_VERSION/meson-$MESON_VERSION.tar.gz"
printf '%s  %s\n' "$XKBCOMMON_SHA256" xkbcommon.tar.gz "$MESON_SHA256" meson.tar.gz |
    sha256sum --check --strict
tar xzf xkbcommon.tar.gz
tar xzf meson.tar.gz

python3 "meson-$MESON_VERSION/meson.py" setup build "libxkbcommon-xkbcommon-$XKBCOMMON_VERSION" \
    --prefix=/usr \
    --buildtype=release \
    -Ddefault_library=static \
    -Db_staticpic=true \
    -Denable-x11=true \
    -Denable-wayland=false \
    -Denable-xkbregistry=false \
    -Denable-tools=false \
    -Denable-docs=false \
    -Denable-bash-completion=false \
    -Dxkb-config-root=/usr/share/X11/xkb \
    -Dxkb-config-extra-path=/etc/xkb \
    -Dx-locale-root=/usr/share/X11/locale
ninja -C build libxkbcommon.a libxkbcommon-x11.a

rm -f "$out/libxkbcommon.a" "$out/libxkbcommon-x11.a"
ar -M <<EOF
CREATE $out/libxkbcommon.a
ADDLIB build/libxkbcommon.a
ADDLIB build/libxkbcommon-x11.a
ADDLIB $xcb_xkb
SAVE
END
EOF
ranlib "$out/libxkbcommon.a"
ln -s libxkbcommon.a "$out/libxkbcommon-x11.a"
echo "static xkbcommon $XKBCOMMON_VERSION with libxcb-xkb: $out/libxkbcommon.a"
