//! Execution: runs a parsed list inside the app process.
//!
//! Every command is an async function over streaming stdin/stdout/stderr
//! (see `commands/`). Pipeline stages run concurrently and exchange bounded
//! chunks, so an unbounded producer stays bounded and a consumer that exits
//! early stops it through a broken-pipe error. Cancellation and timeout work
//! by dropping the whole future in `bash::run`; nothing outlives it.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use super::ShellState;
use super::parser::{Command, List, Pipeline, RedirectKind, Sep};

/// Result of one `bash` tool call.
pub struct RunResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl RunResult {
    /// Used for syntax errors, which never reach a command.
    pub fn error(code: i32, message: String) -> Self {
        Self {
            exit_code: code,
            stdout: String::new(),
            stderr: message,
        }
    }
}

/// What a command returns.
pub struct CommandStatus {
    pub code: i32,
    /// `exit`: stop the rest of this command list.
    pub stop: bool,
}

impl CommandStatus {
    pub fn code(code: i32) -> Self {
        Self { code, stop: false }
    }

    pub fn exit(code: i32) -> Self {
        Self { code, stop: true }
    }
}

/// Boxed future used by the streaming traits.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait Source: Send {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, io::Result<usize>>;
}

pub(crate) trait Sink: Send {
    /// Writes the whole buffer or fails.
    fn write<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, io::Result<()>>;

    /// Flushes buffered data; called after the command finishes.
    fn finish(&mut self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------- sources

pub(crate) struct NullSource;

impl Source for NullSource {
    fn read<'a>(&'a mut self, _buf: &'a mut [u8]) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async { Ok(0) })
    }
}

pub(crate) struct FileSource {
    file: tokio::fs::File,
}

impl FileSource {
    pub(crate) fn new(file: tokio::fs::File) -> Self {
        Self { file }
    }
}

impl Source for FileSource {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move { self.file.read(buf).await })
    }
}

pub(crate) struct PipeSource {
    receiver: mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    offset: usize,
}

impl PipeSource {
    pub(crate) fn new(receiver: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            buffer: Vec::new(),
            offset: 0,
        }
    }
}

impl Source for PipeSource {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            loop {
                if self.offset < self.buffer.len() {
                    break;
                }
                match self.receiver.recv().await {
                    Some(chunk) => {
                        self.buffer = chunk;
                        self.offset = 0;
                    }
                    None => return Ok(0),
                }
            }
            let available = &self.buffer[self.offset..];
            let count = available.len().min(buf.len());
            buf[..count].copy_from_slice(&available[..count]);
            self.offset += count;
            Ok(count)
        })
    }
}

// ------------------------------------------------------------------ sinks

pub(crate) struct NullSink;

impl Sink for NullSink {
    fn write<'a>(&'a mut self, _buf: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) struct FileSink {
    file: tokio::fs::File,
}

impl FileSink {
    pub(crate) fn new(file: tokio::fs::File) -> Self {
        Self { file }
    }
}

impl Sink for FileSink {
    fn write<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.file.write_all(buf).await })
    }

    fn finish(&mut self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { self.file.flush().await })
    }
}

/// Buffers a bounded number of chunks in a channel; sending into a closed
/// channel reports `BrokenPipe`, which is how early-exiting consumers stop
/// producers (the `SIGPIPE` analogue).
pub(crate) struct PipeSink {
    sender: mpsc::Sender<Vec<u8>>,
    buffer: Vec<u8>,
}

const PIPE_CHUNK: usize = 16 * 1024;
const PIPE_QUEUE: usize = 8;

impl PipeSink {
    pub(crate) fn new(sender: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            sender,
            buffer: Vec::new(),
        }
    }
}

pub(crate) fn closed_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the pipe is closed")
}

impl Sink for PipeSink {
    fn write<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.buffer.extend_from_slice(buf);
            if self.buffer.len() >= PIPE_CHUNK {
                let chunk = std::mem::take(&mut self.buffer);
                self.sender.send(chunk).await.map_err(|_| closed_pipe())?;
            }
            Ok(())
        })
    }

    fn finish(&mut self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            if !self.buffer.is_empty() {
                let chunk = std::mem::take(&mut self.buffer);
                self.sender.send(chunk).await.map_err(|_| closed_pipe())?;
            }
            Ok(())
        })
    }
}

/// Writes into the one shared capture buffer of a `bash` call, keeping at
/// most `cap` bytes (extra output is dropped, like the Controller-side
/// truncation of worker output).
#[derive(Clone)]
pub(crate) struct CaptureWriter {
    data: Arc<std::sync::Mutex<Vec<u8>>>,
    cap: usize,
}

impl CaptureWriter {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            data: Arc::new(std::sync::Mutex::new(Vec::new())),
            cap,
        }
    }

    fn push(&self, buf: &[u8]) {
        let mut data = self.data.lock().expect("capture lock poisoned");
        if data.len() < self.cap {
            let room = self.cap - data.len();
            data.extend_from_slice(&buf[..room.min(buf.len())]);
        }
    }

    pub(crate) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.data.lock().expect("capture lock poisoned"))
    }
}

pub(crate) struct CaptureSink {
    writer: CaptureWriter,
}

impl CaptureSink {
    pub(crate) fn new(writer: CaptureWriter) -> Self {
        Self { writer }
    }
}

impl Sink for CaptureSink {
    fn write<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.writer.push(buf);
            Ok(())
        })
    }
}

/// A sink shared between the objects that write to it (pipeline stages,
/// merged stdout/stderr); serializes access.
#[derive(Clone)]
pub(crate) struct SharedSink {
    inner: Arc<Mutex<Box<dyn Sink>>>,
}

impl SharedSink {
    pub(crate) fn new(sink: Box<dyn Sink>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(sink)),
        }
    }

    pub(crate) fn share(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Sink for SharedSink {
    fn write<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            inner.write(buf).await
        })
    }

    fn finish(&mut self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            inner.finish().await
        })
    }
}

// -------------------------------------------------------------------- ctx

/// Everything a command gets: the shell state plus its three streams.
pub(crate) struct Ctx<'a> {
    pub(crate) state: &'a mut ShellState,
    pub(crate) input: &'a mut dyn Source,
    pub(crate) out: &'a mut dyn Sink,
    pub(crate) err: &'a mut dyn Sink,
}

impl Ctx<'_> {
    pub(crate) async fn write_out(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write(bytes).await?;
        Ok(())
    }

    pub(crate) async fn write_err(&mut self, bytes: &[u8]) -> Result<()> {
        self.err.write(bytes).await?;
        Ok(())
    }
}

pub(crate) fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}

/// `cwd` for relative targets.
pub(crate) fn resolve(cwd: &Path, target: &str) -> PathBuf {
    let path = Path::new(target);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

// ---------------------------------------------------------------- running

pub(crate) async fn run_list(list: &List, state: &mut ShellState) -> RunResult {
    let stdout = CaptureWriter::new(state.capture_limit);
    let stderr = CaptureWriter::new(state.capture_limit);
    let out_sink = SharedSink::new(Box::new(CaptureSink::new(stdout.clone())));
    let err_sink = SharedSink::new(Box::new(CaptureSink::new(stderr.clone())));
    let mut status = 0;
    let mut run_next = true;
    for item in &list.items {
        if run_next {
            let (code, stop) =
                run_pipeline(&item.pipeline, state, out_sink.share(), err_sink.share()).await;
            status = code;
            state.last_status = code;
            if stop {
                break;
            }
        }
        match item.sep {
            Sep::End | Sep::Semi => run_next = true,
            Sep::And => run_next = status == 0,
            Sep::Or => run_next = status != 0,
        }
    }
    RunResult {
        exit_code: status,
        stdout: String::from_utf8_lossy(&stdout.take()).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.take()).into_owned(),
    }
}

async fn run_pipeline(
    pipeline: &Pipeline,
    state: &mut ShellState,
    out: SharedSink,
    err: SharedSink,
) -> (i32, bool) {
    if pipeline.commands.len() == 1 {
        let outcome = run_stage(&pipeline.commands[0], state, Box::new(NullSource), out, err).await;
        return (outcome.code, outcome.stop);
    }
    // Pipeline stages run concurrently, each on its own clone of the shell
    // state (POSIX subshell semantics: `cd` inside a pipeline stays there).
    let count = pipeline.commands.len();
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    for _ in 0..count - 1 {
        let (sender, receiver) = mpsc::channel::<Vec<u8>>(PIPE_QUEUE);
        senders.push(sender);
        receivers.push(receiver);
    }
    let mut inputs: Vec<Box<dyn Source>> = Vec::with_capacity(count);
    inputs.push(Box::new(NullSource));
    for receiver in receivers {
        inputs.push(Box::new(PipeSource::new(receiver)));
    }
    let mut outputs: Vec<SharedSink> = Vec::with_capacity(count);
    for sender in senders {
        outputs.push(SharedSink::new(Box::new(PipeSink::new(sender))));
    }
    outputs.push(out);
    let mut stage_states: Vec<ShellState> = (0..count).map(|_| state.clone()).collect();
    let futures = pipeline
        .commands
        .iter()
        .zip(stage_states.iter_mut())
        .zip(inputs.into_iter().zip(outputs))
        .map(|((command, stage_state), (input, output))| {
            let err = err.share();
            async move { run_stage(command, stage_state, input, output, err).await }
        })
        .collect::<Vec<_>>();
    let outcomes = futures_util::future::join_all(futures).await;
    let code = outcomes.last().map(|outcome| outcome.code).unwrap_or(0);
    let stop = outcomes.iter().any(|outcome| outcome.stop);
    (code, stop)
}

struct StageOutcome {
    code: i32,
    stop: bool,
}

enum ErrTarget {
    File { append: bool, target: String },
    Merge,
}

async fn run_stage(
    command: &Command,
    state: &mut ShellState,
    mut input: Box<dyn Source>,
    out_base: SharedSink,
    err_base: SharedSink,
) -> StageOutcome {
    // Setup progress and redirect-open failures go to the stage's base
    // stderr (the capture, or the shared stderr for pipeline stages).
    let mut setup_err = err_base.share();

    // 1. Leading `NAME=value` words assign; values expand like any word.
    let mut assignments: Vec<(String, String)> = Vec::new();
    let mut index = 0;
    while index < command.words.len() {
        let Some((name, value_raw)) = split_assignment(&command.words[index]) else {
            break;
        };
        match super::expand::expand_word(&value_raw, state, &mut setup_err).await {
            Ok(value) => assignments.push((name, value.unwrap_or_default())),
            Err(error) => return fail(&mut setup_err, 2, format!("sh: {error}")).await,
        }
        index += 1;
    }

    // 2. Expand the remaining words.
    let mut args: Vec<String> = Vec::new();
    for raw in &command.words[index..] {
        match super::expand::expand_word(raw, state, &mut setup_err).await {
            Ok(Some(word)) => args.push(word),
            Ok(None) => {}
            Err(error) => return fail(&mut setup_err, 2, format!("sh: {error}")).await,
        }
    }

    // 3. Redirections; the last one per descriptor wins. `2>&1` merges into
    // whatever stdout currently is.
    let mut stdout_target: Option<(bool, String)> = None;
    let mut stderr_target: Option<ErrTarget> = None;
    let mut stdin_target: Option<String> = None;
    for redirect in &command.redirects {
        if redirect.kind == RedirectKind::StderrToStdout {
            stderr_target = Some(ErrTarget::Merge);
            continue;
        }
        let target = match super::expand::expand_word(&redirect.target, state, &mut setup_err).await
        {
            Ok(Some(target)) if !target.is_empty() => target,
            Ok(_) => {
                return fail(
                    &mut setup_err,
                    1,
                    format!("sh: ambiguous redirect: {}", redirect.target),
                )
                .await;
            }
            Err(error) => return fail(&mut setup_err, 2, format!("sh: {error}")).await,
        };
        match redirect.kind {
            RedirectKind::Stdout | RedirectKind::StdoutAppend => {
                let append = redirect.kind == RedirectKind::StdoutAppend;
                // `/dev/stdout` means the current stdout: keep it as is.
                stdout_target = (target != "/dev/stdout").then_some((append, target));
            }
            RedirectKind::Stderr | RedirectKind::StderrAppend => {
                let append = redirect.kind == RedirectKind::StderrAppend;
                stderr_target = if target == "/dev/stdout" {
                    Some(ErrTarget::Merge)
                } else if target == "/dev/stderr" {
                    None
                } else {
                    Some(ErrTarget::File { append, target })
                };
            }
            RedirectKind::Stdin => stdin_target = Some(target),
            RedirectKind::StderrToStdout => unreachable!("handled above"),
        }
    }

    let command_name = args.first().cloned().unwrap_or_default();

    let mut out_handle = match stdout_target {
        Some((_, ref target)) if target == "/dev/stderr" => err_base.share(),
        Some((append, ref target)) => match open_output(state, append, target).await {
            Ok(Some(file)) => SharedSink::new(Box::new(FileSink::new(file))),
            Ok(None) => SharedSink::new(Box::new(NullSink)),
            Err(error) => {
                return fail(
                    &mut setup_err,
                    1,
                    format!("{command_name}: cannot open {target}: {error}"),
                )
                .await;
            }
        },
        None => out_base.share(),
    };
    let mut err_handle = match stderr_target {
        Some(ErrTarget::Merge) => out_handle.share(),
        Some(ErrTarget::File { append, target }) => {
            match open_output(state, append, &target).await {
                Ok(Some(file)) => SharedSink::new(Box::new(FileSink::new(file))),
                Ok(None) => SharedSink::new(Box::new(NullSink)),
                Err(error) => {
                    return fail(
                        &mut setup_err,
                        1,
                        format!("{command_name}: cannot open {target}: {error}"),
                    )
                    .await;
                }
            }
        }
        None => err_base.share(),
    };
    let mut input_slot: Option<Box<dyn Source>> = None;
    if let Some(target) = stdin_target {
        if target == "/dev/null" {
            input_slot = Some(Box::new(NullSource));
        } else {
            match tokio::fs::File::open(resolve(&state.cwd, &target)).await {
                Ok(file) => input_slot = Some(Box::new(FileSource::new(file))),
                Err(error) => {
                    return fail(
                        &mut setup_err,
                        1,
                        format!("{command_name}: cannot open {target}: {error}"),
                    )
                    .await;
                }
            }
        }
    }

    // 4. Assignments with no command persist in the shell (after the
    // redirections above, so `> file` still creates the file).
    if args.is_empty() {
        for (name, value) in assignments {
            state.vars.insert(name, value);
        }
        let _ = out_handle.finish().await;
        let _ = err_handle.finish().await;
        return StageOutcome {
            code: 0,
            stop: false,
        };
    }

    // 5. Assignments in front of a command apply to that command: to the
    // shell itself for the builtins defined to keep them, otherwise to a
    // temporary overlay that is dropped afterwards.
    let mut overlay: Option<ShellState> = None;
    if !assignments.is_empty() {
        if keeps_assignments(&command_name) {
            for (name, value) in &assignments {
                state.vars.insert(name.clone(), value.clone());
            }
        } else {
            let mut copy = state.clone();
            for (name, value) in &assignments {
                copy.vars.insert(name.clone(), value.clone());
            }
            overlay = Some(copy);
        }
    }
    let exec_state = match overlay.as_mut() {
        Some(copy) => copy,
        None => state,
    };

    let mut out_ctx = out_handle.share();
    let mut err_ctx = err_handle.share();
    let ctx_input: &mut dyn Source = match input_slot.as_mut() {
        Some(source) => source.as_mut(),
        None => input.as_mut(),
    };
    let mut ctx = Ctx {
        state: exec_state,
        input: ctx_input,
        out: &mut out_ctx,
        err: &mut err_ctx,
    };
    let result = super::commands::run(&command_name, &mut ctx, &args[1..]).await;
    let _ = &ctx;

    let _ = out_handle.finish().await;
    let outcome = match result {
        Ok(status) => StageOutcome {
            code: status.code,
            stop: status.stop,
        },
        Err(error) if is_broken_pipe(&error) => StageOutcome {
            code: 141,
            stop: false,
        },
        Err(error) => {
            let _ = err_handle.write(format!("{error}\n").as_bytes()).await;
            StageOutcome {
                code: 1,
                stop: false,
            }
        }
    };
    let _ = err_handle.finish().await;
    outcome
}

async fn fail(err: &mut SharedSink, code: i32, message: String) -> StageOutcome {
    let _ = err.write(message.as_bytes()).await;
    let _ = err.write(b"\n").await;
    StageOutcome { code, stop: false }
}

async fn open_output(
    state: &ShellState,
    append: bool,
    target: &str,
) -> io::Result<Option<tokio::fs::File>> {
    if target == "/dev/null" {
        return Ok(None);
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options.open(resolve(&state.cwd, target)).await.map(Some)
}

/// Builtins whose `NAME=value` prefixes stay in the shell (POSIX special
/// builtins, plus the ones that would otherwise lose their effect).
fn keeps_assignments(name: &str) -> bool {
    matches!(
        name,
        ":" | "cd" | "eval" | "exit" | "export" | "set" | "sh" | "unset"
    )
}

fn split_assignment(raw: &str) -> Option<(String, String)> {
    let (name, value) = raw.split_once('=')?;
    if !super::expand::valid_name(name) {
        return None;
    }
    Some((name.to_owned(), value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{ShellState, run};

    fn state() -> (tempfile::TempDir, ShellState) {
        let dir = tempfile::tempdir().unwrap();
        let state = ShellState::new(dir.path().to_path_buf(), "test".to_owned());
        (dir, state)
    }

    #[tokio::test]
    async fn runs_lists_with_and_or() {
        let (_dir, mut state) = state();
        let result = run("false || echo b; true && echo a", &mut state).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "b\na\n");

        let result = run("false && echo x", &mut state).await;
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.stdout, "");

        let result = run("false || echo a && echo b", &mut state).await;
        assert_eq!(result.stdout, "a\nb\n");
    }

    #[tokio::test]
    async fn pipelines_run_the_last_status_and_stream() {
        let (_dir, mut state) = state();
        let result = run("echo hello | tr a-z A-Z", &mut state).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "HELLO\n");

        let result = run("false | true", &mut state).await;
        assert_eq!(result.exit_code, 0);

        let result = run("echo x | false", &mut state).await;
        assert_eq!(result.exit_code, 1);
    }

    #[tokio::test]
    async fn exit_stops_the_list() {
        let (_dir, mut state) = state();
        let result = run("echo a; exit 3; echo b", &mut state).await;
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.stdout, "a\n");
    }

    #[tokio::test]
    async fn assignments_follow_posix_visibility() {
        let (_dir, mut state) = state();
        let result = run("A=1; echo $A", &mut state).await;
        assert_eq!(result.stdout, "1\n");

        // Expanded before the assignment applies (POSIX), and the overlay is
        // gone afterwards.
        let result = run("B=2 echo $B", &mut state).await;
        assert_eq!(result.stdout, "\n");
        let result = run("echo $B", &mut state).await;
        assert_eq!(result.stdout, "\n");

        // Around a builtin that keeps them, they persist.
        let result = run("C=3 export C; echo $C", &mut state).await;
        assert_eq!(result.stdout, "3\n");
    }

    #[tokio::test]
    async fn redirections_write_files_and_merge_stderr() {
        let (_dir, mut state) = state();
        let result = run("echo a > f; echo b >> f; cat f", &mut state).await;
        assert_eq!(result.stdout, "a\nb\n");

        let result = run("cat missing 2>&1", &mut state).await;
        assert_eq!(result.exit_code, 1);
        assert!(result.stdout.contains("missing"));
        assert!(result.stderr.is_empty());

        let result = run("echo hidden > /dev/null", &mut state).await;
        assert_eq!(result.stdout, "");
    }

    #[tokio::test]
    async fn single_command_cd_mutates_the_shell_but_pipeline_cd_does_not() {
        let (dir, mut state) = state();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        run("cd sub", &mut state).await;
        assert_eq!(state.cwd, sub);
        run("cd .. | cat", &mut state).await;
        assert_eq!(state.cwd, sub);
        let result = run("pwd", &mut state).await;
        assert_eq!(result.stdout.trim_end(), sub.display().to_string());
    }

    #[tokio::test]
    async fn command_substitution_feeds_words() {
        let (_dir, mut state) = state();
        let result = run("echo $(echo inner)", &mut state).await;
        assert_eq!(result.stdout, "inner\n");
    }

    #[tokio::test]
    async fn unknown_commands_exit_127() {
        let (_dir, mut state) = state();
        let result = run("definitely-not-a-command", &mut state).await;
        assert_eq!(result.exit_code, 127);
        assert!(result.stderr.contains("command not found"));
    }

    #[tokio::test]
    async fn capture_truncates_past_the_limit() {
        let writer = CaptureWriter::new(4);
        let mut sink = CaptureSink::new(writer.clone());
        sink.write(b"abcdefgh").await.unwrap();
        assert_eq!(writer.take(), b"abcd");
    }

    #[tokio::test]
    async fn closed_pipe_reports_broken_pipe() {
        let (sender, receiver) = mpsc::channel::<Vec<u8>>(1);
        drop(receiver);
        let mut sink = PipeSink::new(sender);
        let error = sink.write(&vec![0u8; PIPE_CHUNK]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
