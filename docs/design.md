# Design

The app is a Cybion Worker: it receives `tool_call` events from the Controller
over an outbound SSE connection and posts results back over HTTPS. There are no
listening sockets on the phone.

## Layers

| Layer | Language | Responsibility |
| --- | --- | --- |
| Protocol core | Rust | Pairing, SSE session, delivery receipts, dedup, retries, heartbeats, resources, tool dispatch |
| Command engine | Rust | The `bash` tool: a POSIX-style interpreter and native commands, all in-process |
| Platform bridge | Rust -> Swift (C ABI) | Opening URLs (the only thing that needs UIKit) |
| iOS shell | Swift (SwiftUI) | Single screen, background keep-alive, pairing links, status polling |
| Build | XcodeGen + cargo | One app containing the Rust `staticlib` |

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

`bash` runs an in-process command engine (`rust/src/shell/`, documented in
[commands.md](commands.md)): each call starts a fresh shell with `HOME` and
the working directory set to `Documents/CybionWorker` (shared with the Files
app), runs the parsed list, and returns `{exit_code, stdout, stderr}` with
output capped at 2 MiB.

- Pipeline stages run concurrently and exchange bounded chunks; a consumer
  that exits early stops the producer through a broken-pipe error, and an
  unbounded producer stays bounded. Pipeline stages and `$(...)` run on
  cloned state, so `cd` inside them never leaks out.
- The timeout and cancellation drop the whole engine future: nothing keeps
  running after a `cancel` event or a timeout, and both end the call with an
  explicit tool error, matching the desktop Worker's messages.
- Unsupported grammar or commands fail with explicit errors instead of
  guessing; see `docs/commands.md` for the exact subset.

`computer_use` supports `open` and answers every other action with an explicit
"not supported on iOS Workers" error. The desktop action names stay the same;
iOS simply has no accessibility-style API for other apps.

## Decisions

- **In-process command engine instead of a bundled shell.** iOS refuses to
  `exec` any binary from a sandboxed app - `posix_spawn` fails with `EPERM`
  even for a binary inside the app bundle - so a packaged shell could never
  run on a device. Commands therefore execute inside the app process, the
  same approach a-Shell uses. (The simulator *can* spawn, but running a
  different shell there would only hide device bugs, so both use the engine.)
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
