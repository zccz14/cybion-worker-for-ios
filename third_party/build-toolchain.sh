#!/bin/sh
# Rebuilds the iOS userland bundled inside the app, for both the device and
# the simulator runtime:
#   bin/mksh-sh        device   / bin/sim/mksh-sh        simulator
#   bin/busybox        device   / bin/sim/busybox        simulator
#
# mksh is the MirBSD Korn shell (the shell family Android exposes as
# /system/bin/sh); busybox is the multicall utility set (toybox's role on
# Android). Both are pinned below.
#
# Requires: Xcode, git. Usage: sh build-toolchain.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)

MKSH_REPO=https://github.com/MirBSD/mksh
MKSH_COMMIT=0fc6e3f2dc1f5732bd8b81d54f5b5e75d85e4c76
BUSYBOX_REPO=https://github.com/mirror/busybox
BUSYBOX_TAG=1_36_1

fetch() {
    if [ ! -d "$work/mksh" ]; then
        git clone --quiet "$MKSH_REPO" "$work/mksh"
        git -C "$work/mksh" checkout --quiet "$MKSH_COMMIT"
    fi
    if [ ! -d "$work/busybox" ]; then
        git clone --quiet --depth 1 --branch "$BUSYBOX_TAG" "$BUSYBOX_REPO" "$work/busybox"
        git -C "$work/busybox" apply "$here/patches/busybox-ios.patch"
        cp "$here/busybox-ios.config" "$work/busybox/.config"
    fi
}

# $1: device|simulator  $2: output directory
build() {
    platform=$1
    out=$2
    case "$platform" in
        device) sdk=iphoneos; target=arm64-apple-ios17.0 ;;
        simulator) sdk=iphonesimulator; target=arm64-apple-ios17.0-simulator ;;
    esac
    sdk_path=$(xcrun --sdk "$sdk" --show-sdk-path)
    clang="$(xcrun --sdk "$sdk" -f clang) -target $target -isysroot $sdk_path"

    mkdir -p "$out"
    echo "== mksh ($platform) =="
    (
        cd "$work/mksh"
        sh Build.sh -r >/dev/null 2>&1
        cp mksh "$out/mksh-sh"
    )
    echo "== busybox ($platform) =="
    (
        cd "$work/busybox"
        make clean >/dev/null 2>&1 || true
        make -j"$(sysctl -n hw.ncpu)" \
            CC="$clang" HOSTCC=clang AR="$PWD/scripts/arbb.sh" SKIP_STRIP=y >/dev/null
        cp busybox "$out/busybox"
    )
}

fetch
# The same source tree is rebuilt per platform; CC carries the target flags,
# but Build.sh / kbuild both cache probes, so each platform builds in order.
build device "$here/bin"
build simulator "$here/bin/sim"
echo "done:"
ls -la "$here/bin" "$here/bin/sim"
