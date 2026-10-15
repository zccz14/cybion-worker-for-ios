// C surface of the Rust worker core (rust/src/entry.rs). Keep both sides in sync.
#ifndef CYBION_WORKER_H
#define CYBION_WORKER_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// App-side callbacks the worker core cannot implement itself.
/// `open_url` returns 1 on success, 0 on failure.
typedef struct {
    void *context;
    int32_t (*open_url)(void *context, const char *url);
} CybionPlatform;

/// Registers the app-side callbacks; call before `cybion_worker_start`.
void cybion_worker_register_platform(const CybionPlatform *bridge);

/// Starts the worker engine. `config_dir` is the writable container folder
/// that stores worker.toml, the pending pairing and worker.log; `shell_path`
/// and `busybox_path` point into the app bundle. False means the native side
/// rejected the start (details land in worker.log).
bool cybion_worker_start(const char *config_dir,
                         const char *device_info_json,
                         const char *shell_path,
                         const char *busybox_path);

/// Requests shutdown and waits briefly for the engine thread to exit.
void cybion_worker_stop(void);

/// Serialized status snapshot; free the result with `cybion_worker_string_free`.
char *cybion_worker_status(void);

/// Deletes the stored configuration and pending pairing; only while stopped.
bool cybion_worker_reset(const char *config_dir);

/// Releases a string returned by `cybion_worker_status`.
void cybion_worker_string_free(char *value);

#ifdef __cplusplus
}
#endif

#endif
