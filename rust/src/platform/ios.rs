use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail, ensure};

/// The shell and utilities live inside the app bundle; commands run with the
/// app container as their working directory and HOME, and a PATH that finds
/// the bundled `sh` first, then per-container executables the user or agent
/// may drop into `<working_dir>/bin`.
pub struct ShellEnvironment {
    pub shell: PathBuf,
    pub path: String,
    pub working_dir: PathBuf,
}

static SHELL: OnceLock<ShellEnvironment> = OnceLock::new();

pub fn init_shell(shell_path: &str, busybox_path: &str, working_dir: &Path) -> Result<()> {
    let shell = PathBuf::from(shell_path);
    ensure!(
        shell.is_absolute(),
        "the shell path must be absolute: {shell_path}"
    );
    let bin_dir = shell
        .parent()
        .context("the shell path has no parent directory")?;
    let applets_dir = working_dir.join("bin");
    materialize_applets(&applets_dir, &shell, Path::new(busybox_path));
    let path = format!("{}:{}", applets_dir.display(), bin_dir.display());
    let _ = SHELL.set(ShellEnvironment {
        shell,
        path,
        working_dir: working_dir.to_path_buf(),
    });
    Ok(())
}

/// Symlinks `<container>/bin/sh` and one entry per busybox applet, so plain
/// commands (`ls`, `grep`, ...) resolve from PATH exactly like the Android
/// Worker resolves them from `/system/bin`. Best effort: a failure only means
/// those shortcuts are missing, never that the Worker is broken.
fn materialize_applets(applets_dir: &Path, shell: &Path, busybox: &Path) {
    if std::fs::create_dir_all(applets_dir).is_err() {
        return;
    }
    link("sh", shell, applets_dir);
    let Ok(output) = Command::new(busybox).arg("--list").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for applet in String::from_utf8_lossy(&output.stdout).lines() {
        let applet = applet.trim();
        if !applet.is_empty() {
            link(applet, busybox, applets_dir);
        }
    }
}

fn link(name: &str, target: &Path, dir: &Path) {
    let path = dir.join(name);
    if path.symlink_metadata().is_ok() {
        return;
    }
    let _ = std::os::unix::fs::symlink(target, path);
}

pub fn shell_environment() -> Option<&'static ShellEnvironment> {
    SHELL.get()
}

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
