# Protocol

The app speaks the same worker protocol as the desktop `cybion-worker` CLI and
the Android app: outbound HTTPS only, no listening sockets. This document
describes the wire behavior the app implements.

## Identity and pairing

- The device generates two secrets locally: `device_secret` (proves possession
  of the pending pairing) and `access_token` (the long-lived worker
  credential). Only their hashes travel to the controller.
- Pairing starts with `POST /worker/v1/pairings` carrying `device_secret`,
  `token_hash`, `hostname`, `platform`, and `version`. The reply contains a
  `user_code` (12 hex characters, displayed as `XXXX-XXXX-XXXX`) with a
  600-second expiry.
- The device polls `GET /worker/v1/pairings/{id}` (bearer `device_secret`)
  every 3 seconds until the status becomes `approved`, `cancelled`, or the
  pairing expires (HTTP 410). The pending pairing is persisted in
  `worker.pairing.json`, so a restart resumes polling instead of minting a new
  code.
- The user approves the code in the Cybion console. The app deep-links to
  `https://cybion.ntnl.io/#/workers?code=…` from the pairing card.

## Event stream

`GET /worker/v1/users/{user_id}/workers/{worker_id}/events` (SSE) carries all
server-to-device traffic. Headers: `Authorization: Bearer <access_token>`,
`x-cybion-worker-boot-id`, and `x-cybion-worker-version`.

| Event | Payload | Meaning |
| --- | --- | --- |
| `tool_call` | `{id, thread_id, name, arguments}` | execute a tool; checks ride the same event |
| `cancel` | `{id}` | abort a running call |
| `upgrade` | `{id, version, boot_id}` | answered with `failed`; iOS cannot self-install |
| `heartbeat` | `{}` | keep-alive marker |

- A 45-second idle timeout ends the session and triggers reconnect with bounded
  exponential backoff, capped at 30 seconds and jittered.
- The **boot id** is a fresh UUID per worker process. The controller uses it to
  deduplicate deliveries and, when the boot changes, fails calls that the
  previous process had not acknowledged (`execution_outcome: unknown`). While
  a restart is being noticed, one transient `409 report from an obsolete
  Worker process` can appear on `POST .../heartbeat`; the next heartbeat
  succeeds.

## Delivery, receipts, and results

For every received call (duplicates included) the device posts a receipt to
`POST .../{checks|calls}/{call_id}/received`, executes the tool, and posts the
result to `POST .../{checks|calls}/{call_id}/result` as
`{"result": …, "failed": bool}`. Both posts are retried until confirmed, and
`Retry-After` responses are honored.

- `checks` is the route family for `diagnostics`; every other tool uses
  `calls`.
- **Deduplication**: a call is executed once per boot, keyed by the call ID
  plus a SHA-256 fingerprint of `{thread_id, name, arguments}`. A replayed ID
  with identical arguments is skipped; a replayed ID with different arguments
  is dropped with a warning.
- **Cancellation**: `cancel` events signal per-call watch channels. `bash`
  kills the process group with `SIGKILL`; calls not yet started exit before
  running.

## Upgrades

The console can queue a target version for the Worker; the Controller repeats
the `upgrade` event until the status changes. iOS Workers cannot install a new
build of themselves, so the app answers each upgrade id once with

```
POST .../upgrade   {"id": …, "status": "failed", "error": "iOS Workers cannot upgrade themselves; …"}
```

and stops there. Installing a new version is manual (Xcode or a sideload tool).

## Liveness

Every 10 seconds: `POST .../heartbeat` with `{hostname, version}` and
`POST .../resources` with `{logical_cpus, cpu_usage_percent,
memory_used_bytes, memory_total_bytes}` (CPU from `host_statistics`, memory
from `host_statistics64`). The controller marks a worker offline when the last
report is stale.

## Version floor

The controller enables worker checks (diagnostics) for workers at version
`0.1.4` or newer. The app implements the full checks route family, and its
version tracks that supported-worker floor.
