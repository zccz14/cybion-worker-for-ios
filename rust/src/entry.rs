use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use crate::engine::{self, DeviceInfo};
use crate::platform;

/// C surface exposed to the Swift app shell (see `cybion_worker.h`).
#[repr(C)]
pub struct CybionPlatform {
    pub context: *mut c_void,
    pub open_url: Option<platform::OpenUrlFn>,
}

fn string_of(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: the caller (the Swift shell) passes NUL-terminated strings that
    // stay valid for the duration of the call.
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Registers the app-side callbacks; call before `cybion_worker_start`.
#[unsafe(no_mangle)]
pub extern "C" fn cybion_worker_register_platform(bridge: *const CybionPlatform) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the pointer comes from the Swift shell during startup; a
        // null value is handled here, and non-null values point to a live
        // CybionPlatform for the duration of this call.
        let Some(bridge) = (unsafe { bridge.as_ref() }) else {
            log::warn!(target: "cybion_worker", "register_platform called with a null bridge");
            return;
        };
        platform::register_bridge(bridge.context, bridge.open_url);
    }));
}

/// Starts the worker engine; false means the native side rejected the start.
#[unsafe(no_mangle)]
pub extern "C" fn cybion_worker_start(
    config_dir: *const c_char,
    device_info_json: *const c_char,
    shell_path: *const c_char,
    busybox_path: *const c_char,
) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<()> {
        let config_dir =
            string_of(config_dir).ok_or_else(|| anyhow::anyhow!("config dir is missing"))?;
        let device_json =
            string_of(device_info_json).ok_or_else(|| anyhow::anyhow!("device info is missing"))?;
        let shell_path =
            string_of(shell_path).ok_or_else(|| anyhow::anyhow!("shell path is missing"))?;
        let busybox_path =
            string_of(busybox_path).ok_or_else(|| anyhow::anyhow!("busybox path is missing"))?;
        let device: DeviceInfo = serde_json::from_str(&device_json)?;
        let config_dir = PathBuf::from(config_dir);
        std::fs::create_dir_all(&config_dir)?;
        crate::logging::install(&config_dir);
        platform::init_shell(&shell_path, &busybox_path, &config_dir)?;
        engine::start(config_dir, device)
    }));
    match result {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            log::error!(target: "cybion_worker", "start failed: {error:#}");
            false
        }
        Err(_) => {
            log::error!(target: "cybion_worker", "start panicked");
            false
        }
    }
}

/// Requests shutdown and waits briefly for the engine thread to exit.
#[unsafe(no_mangle)]
pub extern "C" fn cybion_worker_stop() {
    let _ = catch_unwind(AssertUnwindSafe(engine::stop));
}

/// Returns the serialized status snapshot; free it with `cybion_worker_string_free`.
#[unsafe(no_mangle)]
pub extern "C" fn cybion_worker_status() -> *mut c_char {
    let payload = match serde_json::to_string(&engine::snapshot()) {
        Ok(payload) => payload,
        Err(error) => serde_json::json!({"error": error.to_string()}).to_string(),
    };
    match CString::new(payload) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Releases a string returned by `cybion_worker_status`.
#[unsafe(no_mangle)]
pub extern "C" fn cybion_worker_string_free(value: *mut c_char) {
    if !value.is_null() {
        // SAFETY: the pointer comes from CString::into_raw above and is freed
        // exactly once by the caller.
        drop(unsafe { CString::from_raw(value) });
    }
}

/// Deletes the stored configuration and pending pairing; only while stopped.
#[unsafe(no_mangle)]
pub extern "C" fn cybion_worker_reset(config_dir: *const c_char) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<()> {
        let config_dir =
            string_of(config_dir).ok_or_else(|| anyhow::anyhow!("config dir is missing"))?;
        engine::reset(&PathBuf::from(config_dir))
    }));
    match result {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            log::error!(target: "cybion_worker", "reset failed: {error:#}");
            false
        }
        Err(_) => false,
    }
}
