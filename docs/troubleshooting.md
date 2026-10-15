# Troubleshooting

Logs live in `Documents/CybionWorker/worker.log` inside the app container.
The container is visible in the **Files** app (On My iPhone → Cybion Worker),
or via:

```sh
xcrun devicectl device copy from --device <UDID> \
    --domain-type appDataContainer --domain-identifier io.ntnl.cybion.worker \
    --source Documents/CybionWorker/worker.log --destination ./worker.log
```

## The app does not launch from the command line

```
The request was denied by service delegate (SBMainWorkspace) for reason: Locked
```

Unlock the phone and launch again. Installs work while locked; cold launches
do not. This only affects the very first launch; afterwards the keep-alive
keeps the process running.

## `shell self-check failed` in worker.log

At startup the worker runs `echo cybion-worker-check` through the in-process
command engine. A failure here means the engine is broken - this should not
happen in a released build. Rebuild and reinstall the app; if it persists,
the log line contains the engine's own error message, which is the report.

## `bash` rejects a command

The engine implements a documented subset of `sh`; unsupported syntax and
commands fail with an explicit error naming what was rejected (loops,
globbing, heredocs, arithmetic, ...). Check [commands.md](commands.md) for
what is supported.

## Pairing problems

- `Pairing expired. 配对已过期，请重新开启。` - codes last ten minutes; tap
  **Reset pairing** (only while stopped), then enable again for a fresh code.
- `Pairing cancelled` - someone rejected the code in the console; same remedy.
- The phone shows a code but the console cannot find it: make sure you enter
  all twelve characters; the console accepts `XXXX-XXXX-XXXX` or the raw code.

## The worker went offline / stopped responding

Open the app: it restarts a dead worker thread whenever the switch is on.
Check `worker.log` for `event stream connected` and recent heartbeats; a
`reconnecting` status means the network went away and it is retrying with
backoff.

## Remote upgrade shows as failed

Expected: iOS Workers cannot replace their own build. Install the new version
from Xcode, then remove the queued upgrade in the console.

## Re-pair with another account

Stop the worker, tap **Reset pairing**, then enable again. The reset deletes
`worker.toml` and any pending pairing; `worker.log` stays for diagnosis.
