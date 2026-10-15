# iOS notes: grants, provisioning, and limits

There is no permission checklist like on Android: the app asks for nothing at
runtime. What matters on iOS is the install and the way the OS treats a
backgrounded app.

## The three things that matter

| Thing | Why it matters |
| --- | --- |
| Development signing | The app installs only with a development identity (a personal team works). Personal-team provisioning expires; re-run from Xcode to renew. |
| Unlocked device | A cold launch is refused while the phone is locked (`The request was denied ... because the device was not, or could not be, unlocked`). Background **re**launches are fine. |
| The switch | The app is a remote-control agent for its own sandbox; the switch (plus the silent audio session it starts) is the user-visible "this phone accepts tasks" contract. |

## Background keep-alive

While **Enable Worker** is on the app holds a `playback` audio session
playing a silent buffer (`UIBackgroundModes: audio`). This is what keeps the
SSE connection alive when the phone is not showing the app; expect a small
battery cost. iOS may still stop the process in edge cases - open the app to
bring the worker back. It reconnects automatically otherwise.

## What iOS does not allow (and how the Worker reports it)

- **Controlling other apps**: no accessibility service exists for third-party
  apps, so `computer_use` taps/swipes/typing are rejected with an explicit
  error. `open` works through the system.
- **Screenshots**: a sandboxed app cannot capture the screen; the action
  reports an explicit error. (`diagnostics` reports `desktop` as
  `unsupported / no_accessibility_api`.)
- **Browser control**: no browser automation hooks; reported as
  `missing_dependency / no_browser`, same shape as Android.
- **Spawning processes**: a sandboxed app cannot `exec` anything - not even a
  binary shipped inside its own bundle (`posix_spawn` fails with `EPERM`).
  There is therefore no system shell: `bash` is an in-process command engine
  (see [commands.md](commands.md)), and there is no `sudo`, no other apps'
  data, no system paths.
