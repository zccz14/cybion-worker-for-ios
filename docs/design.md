# Design

The app is a Cybion Worker: it receives `tool_call` events from the Controller
over an outbound SSE connection and posts results back over HTTPS. There are no
listening sockets on the phone.

## Layers

| Layer | Language | Responsibility |
| --- | --- | --- |
| Protocol core | Rust | Pairing, SSE session, delivery receipts, dedup, retries, heartbeats, resources, tool dispatch |
| Platform bridge | Rust -> Swift (C ABI) | Opening URLs; the shell/userland paths from the app bundle |
| iOS shell | Swift (SwiftUI) | Single screen, background keep-alive, pairing links, status polling |
| Bundled userland | C | `mksh` as `bin/mksh-sh` and `bin/busybox`; re-signed with the app |
| Build | XcodeGen + cargo | One app containing the Rust `staticlib` and the userland |

## Wire protocol (as implemented)

| Step | Request |
| --- | --- |
| Pair | `POST /worker/v1/pairings` then poll `GET /worker/v1/pairings/{id}` |
| Events | `GET /worker/v1/users/{user}/workers/{worker}/events` (SSE, `x-cybion-worker-boot-id`, `x-cybion-worker-version`) |
| Receipt | `POST .../{checks\|calls}/{call_id}/received` |
| Result | `POST .../{checks\|calls}/{call_id}/result` |
| Liveness | `POST .../heartbeat` and `POST .../resources` every 10 s |
| Upgrade | answered once per upgrade id with `failed` and a reason (see below) |

## Tool coverage

`bash` spawns the bundled `mksh` with `-c` in the app container:

- `PATH` = `<container>/bin` then `<bundle>/bin`; at startup the app
  materializes one symlink per busybox applet plus `sh`, so plain commands
  (`ls`, `grep`, `awk`, ...) resolve like they do on Android from `/system/bin`.
- `HOME` and the working directory are `Documents/CybionWorker`, which is
  shared with the Files app.
- Timeout and cancellation kill the whole process group, same as Android.

`computer_use` supports `open` and answers every other action with an explicit
"not supported on iOS Workers" error. The desktop action names stay the same;
iOS simply has no accessibility-style API for other apps.

## Decisions

- **Bundled shell and userland.** iOS gives sandboxed apps no system shell and
  the kernel refuses to execute binaries outside the app bundle, so the shell
  (`mksh`, permissively licensed) and utilities (`busybox`) ship inside the
  app and are re-signed with the app's identity. The applet symlink farm is
  materialized from `busybox --list` at startup so it always matches the
  binary.
- **Silent-audio keep-alive.** iOS suspends ordinary apps seconds after
  backgrounding. While the switch is on the app holds a `playback` audio
  session playing a silent buffer - the standard mechanism for user-enabled
  personal agents. The switch is the user-visible contract, exactly like the
  Android foreground-service notification.
- **No remote upgrade path.** A sandboxed app cannot install a new build of
  itself. The Worker answers a queued upgrade with a single `failed` status
  that says so, instead of pretending or looping.
- **Rust core + minimal Swift shell.** All protocol logic is shared with the
  Android implementation; Swift carries only UI, keep-alive and the URL
  callback. The C surface is four functions plus a callback struct
  (`rust/src/entry.rs`, `ios/CybionWorker/WorkerCore/cybion_worker.h`).
- **The version tracks the controller's supported-worker floor (`0.1.4`).**
  The console only offers checks (diagnostics) to workers at or above it.
