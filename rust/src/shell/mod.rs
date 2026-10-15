//! The in-process command engine behind the `bash` tool.
//!
//! iOS refuses to `exec` any binary from a sandboxed app (even one shipped
//! inside the app bundle), so unlike the desktop and Android Workers there is
//! no child process to run. Commands therefore execute inside the app process:
//! a small POSIX-style interpreter drives a fixed set of native commands.
//! The supported grammar and commands are documented in `docs/commands.md`;
//! anything outside them fails with an explicit error.

mod commands;
mod exec;
mod expand;
mod lexer;
mod parser;

use std::collections::BTreeMap;
use std::path::PathBuf;

pub use exec::RunResult;

/// Most output a single `bash` call returns; extra output is dropped, like
/// the Controller-side truncation in `tools::command_output`.
pub const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

/// Most input a command may buffer in memory (`sort`, `tail`, ...).
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// State threaded through one tool call: each call starts a fresh shell (like
/// `sh -c`), so nothing persists across calls except what commands write to
/// disk. Cloned for pipeline stages (`cd` in a pipeline stays there) and
/// command substitution.
#[derive(Clone)]
pub struct ShellState {
    pub cwd: PathBuf,
    pub vars: BTreeMap<String, String>,
    pub last_status: i32,
    pub hostname: String,
    /// Output capture cap for one `bash` call.
    pub capture_limit: usize,
    /// Input buffer cap for commands that need everything in memory.
    pub buffer_limit: usize,
}

impl ShellState {
    pub fn new(home: PathBuf, hostname: String) -> Self {
        let mut vars = BTreeMap::new();
        vars.insert("HOME".to_owned(), home.display().to_string());
        vars.insert("PATH".to_owned(), ".".to_owned());
        vars.insert("PWD".to_owned(), home.display().to_string());
        vars.insert("SHELL".to_owned(), "sh".to_owned());
        vars.insert("HOSTNAME".to_owned(), hostname.clone());
        vars.insert("USER".to_owned(), "mobile".to_owned());
        vars.insert("LANG".to_owned(), "C".to_owned());
        Self {
            cwd: home,
            vars,
            last_status: 0,
            hostname,
            capture_limit: MAX_CAPTURE_BYTES,
            buffer_limit: MAX_INPUT_BYTES,
        }
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }
}

pub async fn run(command: &str, state: &mut ShellState) -> RunResult {
    let result = run_with_status(command, state).await;
    state.last_status = result.exit_code;
    result
}

/// Used by `$(...)` and `sh -c` recursion; does not touch `last_status`.
pub(crate) async fn run_with_status(command: &str, state: &mut ShellState) -> RunResult {
    match parser::parse(command) {
        Ok(list) => exec::run_list(&list, state).await,
        Err(error) => RunResult::error(2, format!("syntax error: {error}")),
    }
}
