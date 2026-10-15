# Cybion Worker for iOS

Turn an iPhone into a [Cybion](https://cybion.ntnl.io) Worker.

The app keeps an outbound-only SSE connection to the Cybion Controller and
executes tool calls on the device:

- `bash` — an **in-process command engine**: a small POSIX-style interpreter
  plus native commands that run inside the app process. iOS refuses to `exec`
  any binary from a sandboxed app (even one shipped inside the bundle), so
  nothing is spawned; the supported grammar and commands are documented in
  [docs/commands.md](docs/commands.md).
- `computer_use` — `open` launches URLs through the system; iOS gives apps no
  way to drive other apps, so taps, screenshots and UI trees are rejected with
  an explicit error.
- `diagnostics` — the connection check used by the Cybion web console.

`browser_control` is not available and reports an explicit error, like on
Android.

## Status

Early development (`v0.1.x`). Built for personal use against the hosted Cybion
Controller; this is not an App Store app. It installs from Xcode with a
development identity (a personal team works).

## Requirements

- An iPhone on iOS 17 or newer.
- Xcode 26+ with a development team; the device must be unlocked to launch
  the app the first time.
- A Cybion account with a Worker slot available.

## Install

```sh
cd ios
xcodegen generate           # one time; also after changing project.yml
open CybionWorker.xcodeproj # select your team, then Run
```

Or from the command line:

```sh
cd ios && xcodegen generate
xcodebuild -project CybionWorker.xcodeproj -scheme CybionWorker \
    -configuration Release -destination 'generic/platform=iOS' -allowProvisioningUpdates build
xcrun devicectl device install app --device <UDID> \
    ~/Library/Developer/Xcode/DerivedData/CybionWorker-*/Build/Products/Release-iphoneos/CybionWorker.app
```

Prebuilt **unsigned** IPAs are attached to
[releases](https://github.com/zccz14/cybion-worker-for-ios/releases); re-sign
one with your own certificate (Sideloadly, AltStore or Xcode) to install it.

For a fast development loop the iOS Simulator is supported and runs the same
in-process engine as the device:

```sh
sh ios/run-sim.sh
```

## Setup

1. Open the app and turn on **Enable Worker**. The app starts a silent audio
   session so iOS keeps the process alive in the background; turn the switch
   off to stop everything.
2. Approve the pairing: the app shows a 12-character code and a link. Approve
   the device from the Cybion console (`Workers → Connect a device`).
3. Run the connection check in the console, then send tasks to the new Worker.

## Security model

Like the desktop Worker, this app is a remote-control agent, not a sandbox
around what the Controller asks for.

- The connection is outbound HTTPS only; the phone opens no listening ports.
- The access token lives in the app container (`Documents/CybionWorker`), and
  pairing submitted only its SHA-256 hash.
- `bash` runs inside the app sandbox as the app itself: it sees the app's own
  files, not other apps' data and not the system.
- Whoever controls the paired Cybion account controls the app's sandbox. Turn
  the switch off to stop everything.

## Limitations

- `bash` implements a documented subset of `sh`: pipelines, redirections,
  `&&`/`||`, variables and `$(...)`, but no loops, globbing, heredocs or
  background jobs. See [docs/commands.md](docs/commands.md).
- iOS does not let apps control other apps: `computer_use` offers `open` only.
- The background keep-alive uses an audio session; iOS may still stop the
  process occasionally. Open the app to bring the worker back.
- Upgrades are manual: reinstall a new build (Xcode or Xcode-organizer
  workflow). A queued remote upgrade is answered with an explicit failure.
- Installs signed with a personal team expire; re-run from Xcode to renew.

## Development

Toolchain: Rust (pinned in `rust/rust-toolchain.toml`), Xcode, `xcodegen`.
The command engine and its tests live in `rust/src/shell/`.

Checks (`pr-check` runs the same):

```sh
cd rust && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
```

## Documentation

- [docs/design.md](docs/design.md) — architecture and decisions.
- [docs/commands.md](docs/commands.md) — what the `bash` engine accepts.
- [docs/protocol.md](docs/protocol.md) — the wire protocol as implemented.
- [docs/permissions.md](docs/permissions.md) — grants and device notes.
- [docs/troubleshooting.md](docs/troubleshooting.md) — failure modes and fixes.

The wire protocol is the Cybion Worker protocol; sibling implementations live
at [zccz14/cybion-worker](https://github.com/zccz14/cybion-worker) (desktop)
and [zccz14/cybion-worker-for-android](https://github.com/zccz14/cybion-worker-for-android).

## License

MIT.
