//! iOS platform bridge: callbacks only the Swift app shell can perform.
//!
//! The `bash` engine runs in-process (`crate::shell`); this module carries
//! only the interactions that must cross into UIKit, currently opening URLs
//! for the `computer_use` `open` action and the `open` command.

use std::ffi::{CString, c_char, c_void};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

/// Callback into the Swift shell for actions only the app layer can perform.
/// `open_url` returns 1 on success, 0 on failure; the context pointer stays
/// owned by the Swift side.
pub type OpenUrlFn = unsafe extern "C" fn(context: *mut c_void, url: *const c_char) -> i32;

struct Bridge {
    context: *mut c_void,
    open_url: Option<OpenUrlFn>,
}

// SAFETY: the bridge is written once during app startup (before the engine
// starts) and afterwards only read; the Swift side keeps the context alive
// for the process lifetime and makes the callbacks themselves thread-safe.
unsafe impl Send for Bridge {}
unsafe impl Sync for Bridge {}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();

pub fn register_bridge(context: *mut c_void, open_url: Option<OpenUrlFn>) {
    let _ = BRIDGE.set(Bridge { context, open_url });
}

pub fn open_url(url: &str) -> Result<bool> {
    let bridge = BRIDGE
        .get()
        .context("the platform bridge is not registered")?;
    let callback = bridge
        .open_url
        .context("opening URLs is not wired up in the app shell")?;
    let url = CString::new(url).context("the URL contains a null byte")?;
    // SAFETY: the callback and context both come from the Swift shell, which
    // guarantees the context outlives the process; the URL string is valid
    // for the duration of the call.
    let result = unsafe { callback(bridge.context, url.as_ptr()) };
    match result {
        1 => Ok(true),
        0 => Ok(false),
        other => bail!("the app shell returned an unexpected result: {other}"),
    }
}
