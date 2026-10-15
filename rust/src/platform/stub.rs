use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

pub struct ShellEnvironment {
    pub shell: PathBuf,
    pub path: String,
    pub working_dir: PathBuf,
}

pub fn init_shell(_shell_path: &str, _busybox_path: &str, _working_dir: &Path) -> Result<()> {
    Ok(())
}

pub fn shell_environment() -> Option<&'static ShellEnvironment> {
    None
}

pub type OpenUrlFn =
    unsafe extern "C" fn(context: *mut std::ffi::c_void, url: *const std::ffi::c_char) -> i32;

pub fn register_bridge(_context: *mut std::ffi::c_void, _open_url: Option<OpenUrlFn>) {}

pub fn open_url(_url: &str) -> Result<bool> {
    bail!("the platform bridge is only available on iOS")
}
