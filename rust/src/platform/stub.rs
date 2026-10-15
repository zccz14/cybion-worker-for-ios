//! Host build stub: everything the engine cannot do off-device.

pub type OpenUrlFn =
    unsafe extern "C" fn(context: *mut std::ffi::c_void, url: *const std::ffi::c_char) -> i32;

pub fn register_bridge(_context: *mut std::ffi::c_void, _open_url: Option<OpenUrlFn>) {}

pub fn open_url(_url: &str) -> anyhow::Result<bool> {
    anyhow::bail!("the platform bridge is only available on iOS")
}
