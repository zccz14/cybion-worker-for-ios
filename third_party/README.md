# Bundled iOS userland

The app turns the iPhone into a Cybion Worker, so the `bash` tool must execute
**something**. iOS gives sandboxed apps no system shell and cannot spawn
binaries outside their own bundle, so this directory carries the two
executables the app spawns (they are copied into `CybionWorker.app/Resources/bin`
at build time and re-signed with the app's identity):

| Binary | Source | License | Role |
| --- | --- | --- | --- |
| `bin/mksh-sh` | [MirBSD/mksh](https://github.com/MirBSD/mksh) | MirOS licence (BSD-style) | `/bin/sh` equivalent; the same shell family Android exposes as `/system/bin/sh` |
| `bin/busybox` | [busybox 1_36_1](https://github.com/mirror/busybox) | GPLv2 | Multicall utility set (the toybox equivalent on Android) |

Both are arm64 Mach-O executables built against the iPhoneOS SDK. A small
patch set adapts busybox to Darwin/iOS (Apple `ar`, missing GNU functions,
`system()`/`popen()` stubs, Darwin linker flags); see `patches/busybox-ios.patch`.

Rebuild with `sh build-toolchain.sh` (requires Xcode). The built binaries are
committed so an ordinary app build does not depend on the toolchain step.

**License note:** the repository is MIT, but release builds that embed
`busybox` also distribute GPLv2 software; the corresponding source is the
pinned upstream revision plus the patch and config in this directory.
