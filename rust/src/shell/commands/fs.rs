//! Filesystem commands: navigation, listing, copying, and file inspection.

use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};

use super::super::exec::{CommandStatus, Ctx, FileSource, Source, resolve};
use super::{glob_match, read_all, write_line};

fn resolve_in(ctx: &Ctx<'_>, target: &str) -> PathBuf {
    resolve(&ctx.state.cwd, target)
}

fn fail_message(name: &str, path: &str, error: &io::Error) -> String {
    format!("{name}: {path}: {error}")
}

/// Lexically normalizes `a/b/../c` style paths (no symlink resolution).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ------------------------------------------------------------- name games

pub(super) async fn basename(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let Some((path, rest)) = args.split_first() else {
        bail!("basename: missing operand");
    };
    if rest.len() > 1 {
        bail!("basename: extra operand '{}'", rest[1]);
    }
    let stripped = path.trim_end_matches('/');
    let mut name = if stripped.is_empty() {
        "/".to_owned()
    } else {
        stripped.rsplit('/').next().unwrap_or(stripped).to_owned()
    };
    if let Some(suffix) = rest.first()
        && name.ends_with(suffix.as_str())
        && name != *suffix
    {
        name.truncate(name.len() - suffix.len());
    }
    write_line(ctx, &name).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn dirname(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let Some(path) = args.first() else {
        bail!("dirname: missing operand");
    };
    if args.len() > 1 {
        bail!("dirname: extra operand '{}'", args[1]);
    }
    let stripped = path.trim_end_matches('/');
    let dir = match stripped.rfind('/') {
        None => ".".to_owned(),
        Some(0) if stripped.starts_with('/') => "/".to_owned(),
        Some(position) => stripped[..position].to_owned(),
    };
    write_line(ctx, &dir).await?;
    Ok(CommandStatus::code(0))
}

// ------------------------------------------------------------ navigation

pub(super) async fn pwd(ctx: &mut Ctx<'_>, _args: &[String]) -> Result<CommandStatus> {
    let cwd = ctx.state.cwd.display().to_string();
    write_line(ctx, &cwd).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn cd(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let target = match args.first() {
        Some(target) => target.clone(),
        None => ctx
            .state
            .var("HOME")
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("cd: HOME is not set"))?,
    };
    let resolved = normalize(&resolve_in(ctx, &target));
    let meta = std::fs::metadata(&resolved)
        .map_err(|error| anyhow::anyhow!(fail_message("cd", &target, &error)))?;
    if !meta.is_dir() {
        bail!("cd: {target}: Not a directory");
    }
    ctx.state.cwd = resolved.clone();
    ctx.state
        .vars
        .insert("PWD".to_owned(), resolved.display().to_string());
    Ok(CommandStatus::code(0))
}

// --------------------------------------------------------------- reading

async fn copy_stream(input: &mut dyn Source, out: &mut dyn super::super::exec::Sink) -> Result<()> {
    // Heap so the buffer is not part of every command's future state.
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        out.write(&buffer[..count]).await?;
    }
}

pub(super) async fn cat(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let files: Vec<&String> = args.iter().filter(|arg| arg.as_str() != "-u").collect();
    let mut status = 0;
    if files.is_empty() {
        copy_stream(ctx.input, ctx.out).await?;
        return Ok(CommandStatus::code(0));
    }
    for file in files {
        if file.as_str() == "-" {
            copy_stream(ctx.input, ctx.out).await?;
            continue;
        }
        let path = resolve_in(ctx, file);
        match tokio::fs::File::open(&path).await {
            Ok(handle) => {
                let mut source = FileSource::new(handle);
                copy_stream(&mut source, ctx.out).await?;
            }
            Err(error) => {
                let message = fail_message("cat", file, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    Ok(CommandStatus::code(status))
}

pub(super) async fn head(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let (lines, bytes, files) = parse_count("head", args)?;
    let mut status = 0;
    let multiple = files.len() > 1;
    if files.is_empty() {
        head_one(ctx.input, ctx.out, lines, bytes).await?;
        return Ok(CommandStatus::code(0));
    }
    for (index, file) in files.iter().enumerate() {
        let path = resolve_in(ctx, file);
        match tokio::fs::File::open(&path).await {
            Ok(handle) => {
                if multiple {
                    let header = if index == 0 {
                        format!("==> {file} <==\n")
                    } else {
                        format!("\n==> {file} <==\n")
                    };
                    ctx.write_out(header.as_bytes()).await?;
                }
                let mut source = FileSource::new(handle);
                head_one(&mut source, ctx.out, lines, bytes).await?;
            }
            Err(error) => {
                let message = fail_message("head", file, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    Ok(CommandStatus::code(status))
}

async fn head_one(
    input: &mut dyn Source,
    out: &mut dyn super::super::exec::Sink,
    lines: Option<usize>,
    bytes: Option<usize>,
) -> Result<()> {
    let line_limit = lines.unwrap_or(usize::MAX);
    let byte_limit = bytes.unwrap_or(usize::MAX);
    let mut seen_lines = 0usize;
    let mut written = 0usize;
    let mut buffer = vec![0u8; 16 * 1024];
    while written < byte_limit && seen_lines < line_limit {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        let chunk = &buffer[..count];
        let mut take = chunk.len().min(byte_limit - written);
        if seen_lines < line_limit {
            for (position, byte) in chunk.iter().enumerate() {
                if *byte == b'\n' {
                    seen_lines += 1;
                    if seen_lines == line_limit {
                        take = position + 1;
                        break;
                    }
                }
            }
        }
        out.write(&chunk[..take]).await?;
        written += take;
        if take < count {
            return Ok(());
        }
    }
    Ok(())
}

pub(super) async fn tail(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    if args.iter().any(|arg| arg == "-f" || arg == "--follow") {
        bail!("tail: -f is not supported by the in-process shell");
    }
    let (lines, bytes, files) = parse_count("tail", args)?;
    let mut status = 0;
    let multiple = files.len() > 1;
    if files.is_empty() {
        let data = read_all(ctx.input, ctx.state.buffer_limit, "tail").await?;
        write_tail_data(ctx, &data, lines, bytes).await?;
        return Ok(CommandStatus::code(0));
    }
    for (index, file) in files.iter().enumerate() {
        let path = resolve_in(ctx, file);
        match tokio::fs::read(&path).await {
            Ok(data) => {
                if data.len() > ctx.state.buffer_limit {
                    bail!("tail: {file}: file is larger than the buffer limit");
                }
                if multiple {
                    let header = if index == 0 {
                        format!("==> {file} <==\n")
                    } else {
                        format!("\n==> {file} <==\n")
                    };
                    ctx.write_out(header.as_bytes()).await?;
                }
                write_tail_data(ctx, &data, lines, bytes).await?;
            }
            Err(error) => {
                let message = fail_message("tail", file, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    Ok(CommandStatus::code(status))
}

async fn write_tail_data(
    ctx: &mut Ctx<'_>,
    data: &[u8],
    lines: Option<usize>,
    bytes: Option<usize>,
) -> Result<()> {
    if let Some(count) = bytes {
        let start = data.len().saturating_sub(count);
        ctx.write_out(&data[start..]).await?;
        return Ok(());
    }
    let count = lines.unwrap_or(10);
    let ends: Vec<usize> = data
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .map(|(position, _)| position + 1)
        .collect();
    let total = ends.len() + usize::from(!data.is_empty() && !data.ends_with(b"\n"));
    let start = if count >= total {
        0
    } else {
        let first_line = total - count;
        if first_line == 0 {
            0
        } else {
            ends[first_line - 1]
        }
    };
    ctx.write_out(&data[start..]).await?;
    Ok(())
}

fn parse_count(name: &str, args: &[String]) -> Result<(Option<usize>, Option<usize>, Vec<String>)> {
    let mut lines = None;
    let mut bytes = None;
    let mut files = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (kind, attached) = if let Some(rest) = arg.strip_prefix("-n") {
            ('n', Some(rest))
        } else if let Some(rest) = arg.strip_prefix("-c") {
            ('c', Some(rest))
        } else if arg == "-q" || arg == "-v" {
            index += 1;
            continue;
        } else if let Some(digits) = arg.strip_prefix('-')
            && !digits.is_empty()
            && digits.chars().all(|character| character.is_ascii_digit())
        {
            // `head -5` legacy form.
            lines = Some(
                digits
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{name}: invalid number '{digits}'"))?,
            );
            index += 1;
            continue;
        } else if arg.starts_with('-') && arg.len() > 1 {
            bail!("{name}: invalid option '{arg}'");
        } else {
            files.push(arg.clone());
            index += 1;
            continue;
        };
        let value = match attached {
            Some(rest) if !rest.is_empty() => rest.to_owned(),
            _ => {
                index += 1;
                args.get(index)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{name}: option -{kind} requires a value"))?
            }
        };
        let value: usize = value
            .parse()
            .map_err(|_| anyhow::anyhow!("{name}: invalid number '{value}'"))?;
        if kind == 'n' {
            lines = Some(value);
        } else {
            bytes = Some(value);
        }
        index += 1;
    }
    Ok((lines, bytes, files))
}

pub(super) async fn wc(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut want_lines = false;
    let mut want_words = false;
    let mut want_bytes = false;
    let mut want_chars = false;
    let mut files = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    'l' => want_lines = true,
                    'w' => want_words = true,
                    'c' => want_bytes = true,
                    'm' => want_chars = true,
                    _ => bail!("wc: invalid option -- '{letter}'"),
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
    if !(want_lines || want_words || want_bytes || want_chars) {
        want_lines = true;
        want_words = true;
        want_bytes = true;
    }
    let flags = (want_lines, want_words, want_bytes, want_chars);
    let mut totals = [0u64; 4];
    if files.is_empty() {
        let counts = count_stream(ctx.input, flags).await?;
        write_wc(ctx, counts, flags, None).await?;
        return Ok(CommandStatus::code(0));
    }
    let multiple = files.len() > 1;
    let mut status = 0;
    for file in &files {
        let path = resolve_in(ctx, file);
        match tokio::fs::File::open(&path).await {
            Ok(handle) => {
                let mut source = FileSource::new(handle);
                let counts = count_stream(&mut source, flags).await?;
                for (total, count) in totals.iter_mut().zip(counts) {
                    *total += count;
                }
                write_wc(ctx, counts, flags, Some(file)).await?;
            }
            Err(error) => {
                let message = fail_message("wc", file, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    if multiple && status == 0 {
        write_wc(ctx, totals, flags, Some("total")).await?;
    }
    Ok(CommandStatus::code(status))
}

type WcFlags = (bool, bool, bool, bool);

async fn count_stream(input: &mut dyn Source, flags: WcFlags) -> Result<[u64; 4]> {
    let (lines, words, bytes, chars) = flags;
    let mut counts = [0u64; 4];
    let mut in_word = false;
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        if lines {
            counts[0] += chunk.iter().filter(|byte| **byte == b'\n').count() as u64;
        }
        if bytes {
            counts[2] += count as u64;
        }
        if words {
            for byte in chunk {
                if byte.is_ascii_whitespace() {
                    in_word = false;
                } else if !in_word {
                    in_word = true;
                    counts[1] += 1;
                }
            }
        }
        if chars {
            counts[3] += String::from_utf8_lossy(chunk).chars().count() as u64;
        }
    }
    Ok(counts)
}

async fn write_wc(
    ctx: &mut Ctx<'_>,
    counts: [u64; 4],
    flags: WcFlags,
    name: Option<&str>,
) -> Result<()> {
    let mut parts = Vec::new();
    if flags.0 {
        parts.push(format!("{:>7}", counts[0]));
    }
    if flags.1 {
        parts.push(format!("{:>7}", counts[1]));
    }
    if flags.2 {
        parts.push(format!("{:>7}", counts[2]));
    }
    if flags.3 {
        parts.push(format!("{:>7}", counts[3]));
    }
    let mut line = parts.join(" ");
    if let Some(name) = name {
        line.push(' ');
        line.push_str(name);
    }
    write_line(ctx, &line).await
}

// --------------------------------------------------------------- mutation

pub(super) async fn mkdir(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut parents = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg == "-p" {
            parents = true;
        } else if arg.starts_with('-') && arg.len() > 1 {
            bail!("mkdir: invalid option '{arg}'");
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        bail!("mkdir: missing operand");
    }
    let mut status = 0;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        let result = if parents {
            std::fs::create_dir_all(&resolved)
        } else {
            std::fs::create_dir(&resolved)
        };
        if let Err(error) = result {
            let message = format!("mkdir: cannot create directory '{path}': {error}");
            ctx.write_err(format!("{message}\n").as_bytes()).await?;
            status = 1;
        }
    }
    Ok(CommandStatus::code(status))
}

pub(super) async fn rmdir(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let paths: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    if paths.is_empty() {
        bail!("rmdir: missing operand");
    }
    let mut status = 0;
    for path in paths {
        let resolved = resolve_in(ctx, path);
        if let Err(error) = std::fs::remove_dir(&resolved) {
            let message = format!("rmdir: failed to remove '{path}': {error}");
            ctx.write_err(format!("{message}\n").as_bytes()).await?;
            status = 1;
        }
    }
    Ok(CommandStatus::code(status))
}

pub(super) async fn rm(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut recursive = false;
    let mut force = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    'r' | 'R' => recursive = true,
                    'f' => force = true,
                    'd' => {}
                    _ => bail!("rm: invalid option -- '{letter}'"),
                }
            }
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        if force {
            return Ok(CommandStatus::code(0));
        }
        bail!("rm: missing operand");
    }
    let mut status = 0;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        match remove_path(&resolved, recursive) {
            Ok(()) => {}
            Err(error) if force && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                let message = format!("rm: cannot remove '{path}': {error}");
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    Ok(CommandStatus::code(status))
}

fn remove_path(path: &Path, recursive: bool) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if !recursive {
            return Err(io::Error::other("Is a directory"));
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for entry in entries {
            remove_path(&entry, true)?;
        }
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub(super) async fn cp(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let (recursive, paths) = parse_copy_flags("cp", args)?;
    if paths.len() < 2 {
        bail!("cp: missing destination file operand");
    }
    let destination = paths.last().expect("checked above").clone();
    let sources: Vec<String> = paths[..paths.len() - 1].to_vec();
    let destination_resolved = resolve_in(ctx, &destination);
    let destination_is_dir = destination_resolved.is_dir();
    if sources.len() > 1 && !destination_is_dir {
        bail!("cp: target '{destination}' is not a directory");
    }
    let mut status = 0;
    for source in &sources {
        let source_resolved = resolve_in(ctx, source);
        let target = if destination_is_dir {
            destination_resolved.join(source_base_name(source))
        } else {
            destination_resolved.clone()
        };
        if let Err(error) = copy_entry(&source_resolved, &target, recursive) {
            let message = format!("cp: cannot copy '{source}': {error}");
            ctx.write_err(format!("{message}\n").as_bytes()).await?;
            status = 1;
        }
    }
    Ok(CommandStatus::code(status))
}

pub(super) async fn mv(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let (_, paths) = parse_copy_flags("mv", args)?;
    if paths.len() < 2 {
        bail!("mv: missing destination file operand");
    }
    let destination = paths.last().expect("checked above").clone();
    let sources: Vec<String> = paths[..paths.len() - 1].to_vec();
    let destination_resolved = resolve_in(ctx, &destination);
    let destination_is_dir = destination_resolved.is_dir();
    if sources.len() > 1 && !destination_is_dir {
        bail!("mv: target '{destination}' is not a directory");
    }
    let mut status = 0;
    for source in &sources {
        let source_resolved = resolve_in(ctx, source);
        let target = if destination_is_dir {
            destination_resolved.join(source_base_name(source))
        } else {
            destination_resolved.clone()
        };
        let result = std::fs::rename(&source_resolved, &target).or_else(|_| {
            copy_entry(&source_resolved, &target, true)?;
            remove_path(&source_resolved, true)
        });
        if let Err(error) = result {
            let message = format!("mv: cannot move '{source}' to '{destination}': {error}");
            ctx.write_err(format!("{message}\n").as_bytes()).await?;
            status = 1;
        }
    }
    Ok(CommandStatus::code(status))
}

fn parse_copy_flags(name: &str, args: &[String]) -> Result<(bool, Vec<String>)> {
    let mut recursive = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    'r' | 'R' => recursive = true,
                    'f' => {}
                    _ => bail!("{name}: invalid option -- '{letter}'"),
                }
            }
        } else {
            paths.push(arg.clone());
        }
    }
    Ok((recursive, paths))
}

fn copy_entry(source: &Path, target: &Path, recursive: bool) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        let link = std::fs::read_link(source)?;
        let _ = std::fs::remove_file(target);
        std::os::unix::fs::symlink(link, target)
    } else if meta.is_dir() {
        if !recursive {
            return Err(io::Error::other("omitting directory"));
        }
        std::fs::create_dir_all(target)?;
        let mut entries: Vec<_> = std::fs::read_dir(source)?
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_entry(&entry.path(), &target.join(entry.file_name()), true)?;
        }
        Ok(())
    } else {
        std::fs::copy(source, target)?;
        Ok(())
    }
}

pub(super) async fn touch(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut create = true;
    let mut paths = Vec::new();
    for arg in args {
        if arg == "-c" {
            create = false;
        } else if arg.starts_with('-') && arg.len() > 1 {
            bail!("touch: invalid option '{arg}'");
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        bail!("touch: missing file operand");
    }
    let mut status = 0;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        if resolved.symlink_metadata().is_err() {
            if !create {
                continue;
            }
            if let Err(error) = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&resolved)
            {
                let message = format!("touch: cannot touch '{path}': {error}");
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
                continue;
            }
        }
        if let Err(error) = set_mtime_now(&resolved) {
            let message = format!("touch: cannot touch '{path}': {error}");
            ctx.write_err(format!("{message}\n").as_bytes()).await?;
            status = 1;
        }
    }
    Ok(CommandStatus::code(status))
}

fn set_mtime_now(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("the path contains a null byte"))?;
    // SAFETY: `utimes` with a null time array sets both timestamps to now;
    // the path is a valid C string for the duration of the call.
    let result = unsafe { libc::utimes(path.as_ptr(), std::ptr::null()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(super) async fn readlink(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut canonicalize = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg == "-f" {
            canonicalize = true;
        } else if arg.starts_with('-') && arg.len() > 1 {
            bail!("readlink: invalid option '{arg}'");
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        bail!("readlink: missing operand");
    }
    let mut status = 0;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        let result = if canonicalize {
            std::fs::canonicalize(&resolved)
        } else {
            std::fs::read_link(&resolved)
        };
        match result {
            Ok(target) => write_line(ctx, &target.display().to_string()).await?,
            Err(error) => {
                let message = fail_message("readlink", path, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    Ok(CommandStatus::code(status))
}

// --------------------------------------------------------- search & stats

struct FindCriteria {
    name: Option<(String, bool)>,
    kind: Option<char>,
    maxdepth: usize,
    mindepth: usize,
}

pub(super) async fn find(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut index = 0;
    let mut paths: Vec<String> = Vec::new();
    while index < args.len() && !args[index].starts_with('-') {
        paths.push(args[index].clone());
        index += 1;
    }
    if paths.is_empty() {
        paths.push(".".to_owned());
    }
    let mut criteria = FindCriteria {
        name: None,
        kind: None,
        maxdepth: usize::MAX,
        mindepth: 0,
    };
    while index < args.len() {
        let predicate = args[index].clone();
        match predicate.as_str() {
            "-name" | "-iname" => {
                index += 1;
                let pattern = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("find: {predicate} requires a pattern"))?
                    .clone();
                criteria.name = Some((pattern, predicate == "-iname"));
            }
            "-type" => {
                index += 1;
                let kind = args
                    .get(index)
                    .and_then(|value| value.chars().next())
                    .filter(|kind| matches!(kind, 'f' | 'd' | 'l'))
                    .ok_or_else(|| anyhow::anyhow!("find: -type must be f, d or l"))?;
                criteria.kind = Some(kind);
            }
            "-maxdepth" => {
                index += 1;
                criteria.maxdepth = args
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("find: -maxdepth requires a number"))?;
            }
            "-mindepth" => {
                index += 1;
                criteria.mindepth = args
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("find: -mindepth requires a number"))?;
            }
            "-print" => {}
            other => bail!("find: unknown predicate '{other}'"),
        }
        index += 1;
    }
    let mut status = 0;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        let mut matches = Vec::new();
        match find_walk(&resolved, path, 0, &criteria, &mut matches) {
            Ok(()) => {
                let mut output = String::new();
                for entry in matches {
                    output.push_str(&entry);
                    output.push('\n');
                }
                ctx.write_out(output.as_bytes()).await?;
            }
            Err(error) => {
                let message = fail_message("find", path, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    Ok(CommandStatus::code(status))
}

fn find_walk(
    path: &Path,
    display: &str,
    depth: usize,
    criteria: &FindCriteria,
    matches: &mut Vec<String>,
) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    let selected = depth >= criteria.mindepth && {
        let name_matches = match &criteria.name {
            None => true,
            Some((pattern, insensitive)) => {
                let base = display.trim_end_matches('/');
                let base = base.rsplit('/').next().unwrap_or(base);
                if *insensitive {
                    glob_match(&pattern.to_lowercase(), &base.to_lowercase())
                } else {
                    glob_match(pattern, base)
                }
            }
        };
        let kind_matches = match criteria.kind {
            None => true,
            Some('f') => meta.is_file(),
            Some('d') => meta.is_dir(),
            Some('l') => meta.file_type().is_symlink(),
            Some(_) => false,
        };
        name_matches && kind_matches
    };
    if selected {
        matches.push(display.to_owned());
    }
    if meta.is_dir() && depth < criteria.maxdepth {
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            find_walk(
                &entry.path(),
                &join_display(display, &name),
                depth + 1,
                criteria,
                matches,
            )?;
        }
    }
    Ok(())
}

fn join_display(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

pub(super) async fn du(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut summary = false;
    let mut human = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    's' => summary = true,
                    'h' => human = true,
                    _ => bail!("du: invalid option -- '{letter}'"),
                }
            }
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        paths.push(".".to_owned());
    }
    let mut status = 0;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        let mut rows: Vec<(u64, String)> = Vec::new();
        let result = du_walk(&resolved, path, !summary, true, &mut rows);
        match result {
            Ok(_) => {
                let mut output = String::new();
                for (size, display) in rows {
                    let size = if human {
                        human_size(size)
                    } else {
                        size.to_string()
                    };
                    output.push_str(&format!("{size}\t{display}\n"));
                }
                ctx.write_out(output.as_bytes()).await?;
            }
            Err(error) => {
                let message = fail_message("du", path, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
            }
        }
    }
    Ok(CommandStatus::code(status))
}

fn du_walk(
    path: &Path,
    display: &str,
    list_directories: bool,
    top: bool,
    rows: &mut Vec<(u64, String)>,
) -> io::Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut total: u64 = meta.blocks() * 512;
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            total += du_walk(
                &entry.path(),
                &join_display(display, &name),
                list_directories,
                false,
                rows,
            )?;
        }
        if list_directories || top {
            rows.push((total, display.to_owned()));
        }
        Ok(total)
    } else {
        let size = meta.blocks() * 512;
        if top {
            rows.push((size, display.to_owned()));
        }
        Ok(size)
    }
}

// ------------------------------------------------------------- listing

pub(super) async fn ls(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut long = false;
    let mut all = false;
    let mut human = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg == "--" {
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    'l' => long = true,
                    'a' => all = true,
                    'h' => human = true,
                    '1' => {}
                    _ => bail!("ls: invalid option -- '{letter}'"),
                }
            }
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        paths.push(".".to_owned());
    }
    let multiple = paths.len() > 1;
    let mut status = 0;
    let mut printed = false;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        let meta = match std::fs::symlink_metadata(&resolved) {
            Ok(meta) => meta,
            Err(error) => {
                let message = fail_message("ls", path, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
                continue;
            }
        };
        if meta.is_dir() {
            if multiple {
                if printed {
                    ctx.write_out(b"\n").await?;
                }
                ctx.write_out(format!("{path}:\n").as_bytes()).await?;
            }
            match list_directory(&resolved, long, all, human) {
                Ok(lines) => {
                    let mut output = String::new();
                    for line in lines {
                        output.push_str(&line);
                        output.push('\n');
                    }
                    ctx.write_out(output.as_bytes()).await?;
                }
                Err(error) => {
                    let message = fail_message("ls", path, &error);
                    ctx.write_err(format!("{message}\n").as_bytes()).await?;
                    status = 1;
                }
            }
            printed = true;
        } else {
            let line = format_entry(path, &meta, &resolved, long, human);
            ctx.write_out(format!("{line}\n").as_bytes()).await?;
            printed = true;
        }
    }
    Ok(CommandStatus::code(status))
}

fn list_directory(dir: &Path, long: bool, all: bool, human: bool) -> io::Result<Vec<String>> {
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    if all {
        entries.push((".".to_owned(), dir.to_path_buf()));
        if let Some(parent) = dir.parent() {
            entries.push(("..".to_owned(), parent.to_path_buf()));
        }
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !all && name.starts_with('.') {
            continue;
        }
        entries.push((name, entry.path()));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut lines = Vec::with_capacity(entries.len());
    for (name, path) in entries {
        let meta = std::fs::symlink_metadata(&path)?;
        lines.push(format_entry(&name, &meta, &path, long, human));
    }
    Ok(lines)
}

fn format_entry(
    name: &str,
    meta: &std::fs::Metadata,
    path: &Path,
    long: bool,
    human: bool,
) -> String {
    let mut line = if long {
        let size = if human {
            human_size(meta.len())
        } else {
            meta.len().to_string()
        };
        format!(
            "{} {:>2} {} {} {:>8} {} {}",
            mode_string(meta),
            meta.nlink(),
            meta.uid(),
            meta.gid(),
            size,
            format_time(meta_time(meta), "%b %e %H:%M", false),
            name
        )
    } else {
        name.to_owned()
    };
    if meta.file_type().is_symlink()
        && let Ok(target) = std::fs::read_link(path)
    {
        line.push_str(&format!(" -> {}", target.display()));
    }
    line
}

// ---------------------------------------------------------------- stat

pub(super) async fn stat(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut format: Option<String> = None;
    let mut paths = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(inline) = arg.strip_prefix("-c") {
            if inline.is_empty() {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("stat: -c requires a format"))?;
                format = Some(value.clone());
            } else {
                format = Some(inline.to_owned());
            }
        } else if arg != "--" && arg.starts_with('-') && arg.len() > 1 {
            bail!("stat: invalid option '{arg}'");
        } else if arg != "--" {
            paths.push(arg.clone());
        }
        index += 1;
    }
    if paths.is_empty() {
        bail!("stat: missing operand");
    }
    let mut status = 0;
    for path in &paths {
        let resolved = resolve_in(ctx, path);
        let meta = match std::fs::symlink_metadata(&resolved) {
            Ok(meta) => meta,
            Err(error) => {
                let message = fail_message("stat", path, &error);
                ctx.write_err(format!("{message}\n").as_bytes()).await?;
                status = 1;
                continue;
            }
        };
        let text = match &format {
            Some(format) => format_stat(format, path, &meta)?,
            None => default_stat(path, &meta),
        };
        ctx.write_out(text.as_bytes()).await?;
    }
    Ok(CommandStatus::code(status))
}

fn default_stat(path: &str, meta: &std::fs::Metadata) -> String {
    let mut out = String::new();
    out.push_str(&format!("  File: {path}\n"));
    out.push_str(&format!(
        "  Size: {}\tType: {}\n",
        meta.len(),
        type_name(meta)
    ));
    out.push_str(&format!(
        "Access: ({:03o}/{})\n",
        meta.mode() & 0o777,
        mode_string(meta)
    ));
    out.push_str(&format!(
        "Modify: {}\n",
        format_time(meta_time(meta), "%Y-%m-%d %H:%M:%S", false)
    ));
    out
}

fn format_stat(format: &str, path: &str, meta: &std::fs::Metadata) -> Result<String> {
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(character) = chars.next() {
        if character != '%' {
            out.push(character);
            continue;
        }
        let Some(directive) = chars.next() else {
            bail!("stat: the format ends with '%'");
        };
        match directive {
            'n' => out.push_str(path),
            's' => out.push_str(&meta.len().to_string()),
            'a' => out.push_str(&format!("{:o}", meta.mode() & 0o777)),
            'F' => out.push_str(type_name(meta)),
            'Y' => {
                let seconds = meta_time(meta)
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0);
                out.push_str(&seconds.to_string());
            }
            'y' => out.push_str(&format_time(meta_time(meta), "%Y-%m-%d %H:%M:%S", false)),
            '%' => out.push('%'),
            other => bail!("stat: unsupported format directive '%{other}'"),
        }
    }
    out.push('\n');
    Ok(out)
}

fn type_name(meta: &std::fs::Metadata) -> &'static str {
    let file_type = meta.file_type();
    if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symbolic link"
    } else if file_type.is_socket() {
        "socket"
    } else if file_type.is_fifo() {
        "fifo"
    } else if file_type.is_block_device() {
        "block device"
    } else if file_type.is_char_device() {
        "character device"
    } else {
        "regular file"
    }
}

// ----------------------------------------------------------------- test

pub(super) async fn test(
    ctx: &mut Ctx<'_>,
    args: &[String],
    closing: bool,
) -> Result<CommandStatus> {
    let mut args: Vec<String> = args.to_vec();
    if closing {
        if args.last().map(String::as_str) != Some("]") {
            bail!("[: missing ']'");
        }
        args.pop();
    }
    let value = evaluate_test(&args, &ctx.state.cwd)?;
    Ok(CommandStatus::code(if value { 0 } else { 1 }))
}

fn evaluate_test(args: &[String], cwd: &Path) -> Result<bool> {
    match args.len() {
        0 => Ok(false),
        1 => Ok(!args[0].is_empty()),
        2 => {
            if args[0] == "!" {
                Ok(args[1].is_empty())
            } else {
                unary_test(&args[0], &args[1], cwd)
            }
        }
        3 => {
            if args[0] == "!" {
                Ok(!unary_test(&args[1], &args[2], cwd)?)
            } else {
                binary_test(&args[0], &args[1], &args[2])
            }
        }
        _ => {
            if args[0] == "!" {
                Ok(!evaluate_test(&args[1..], cwd)?)
            } else {
                bail!("test: too many arguments")
            }
        }
    }
}

fn unary_test(operator: &str, value: &str, cwd: &Path) -> Result<bool> {
    let path = || resolve(cwd, value);
    match operator {
        "-e" => Ok(path().symlink_metadata().is_ok()),
        "-f" => Ok(std::fs::metadata(path())
            .map(|meta| meta.is_file())
            .unwrap_or(false)),
        "-d" => Ok(std::fs::metadata(path())
            .map(|meta| meta.is_dir())
            .unwrap_or(false)),
        "-s" => Ok(std::fs::metadata(path())
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)),
        "-L" | "-h" => Ok(path()
            .symlink_metadata()
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)),
        "-r" | "-w" | "-x" => Ok(access(&path(), operator)),
        "-z" => Ok(value.is_empty()),
        "-n" => Ok(!value.is_empty()),
        other => bail!("test: unknown operator '{other}'"),
    }
}

fn access(path: &Path, operator: &str) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mode = match operator {
        "-r" => libc::R_OK,
        "-w" => libc::W_OK,
        "-x" => libc::X_OK,
        _ => libc::F_OK,
    };
    // SAFETY: `access` only reads the path, which is a valid C string.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

fn binary_test(left: &str, operator: &str, right: &str) -> Result<bool> {
    match operator {
        "=" | "==" => Ok(left == right),
        "!=" => Ok(left != right),
        "<" => Ok(left < right),
        ">" => Ok(left > right),
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            let left: i64 = left
                .parse()
                .map_err(|_| anyhow::anyhow!("test: {left}: integer expression expected"))?;
            let right: i64 = right
                .parse()
                .map_err(|_| anyhow::anyhow!("test: {right}: integer expression expected"))?;
            Ok(match operator {
                "-eq" => left == right,
                "-ne" => left != right,
                "-lt" => left < right,
                "-le" => left <= right,
                "-gt" => left > right,
                "-ge" => left >= right,
                _ => unreachable!("the outer match selected a comparison"),
            })
        }
        other => bail!("test: unknown operator '{other}'"),
    }
}

// -------------------------------------------------------------- helpers

fn source_base_name(path: &str) -> String {
    if let Some(name) = Path::new(path).file_name() {
        return name.to_string_lossy().into_owned();
    }
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_owned()
}

fn mode_string(meta: &std::fs::Metadata) -> String {
    let file_type = meta.file_type();
    let kind = if file_type.is_dir() {
        'd'
    } else if file_type.is_symlink() {
        'l'
    } else if file_type.is_socket() {
        's'
    } else if file_type.is_fifo() {
        'p'
    } else if file_type.is_block_device() {
        'b'
    } else if file_type.is_char_device() {
        'c'
    } else {
        '-'
    };
    let mode = meta.mode();
    let mut out = String::with_capacity(10);
    out.push(kind);
    for bits in [(mode >> 6) & 7, (mode >> 3) & 7, mode & 7] {
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    out
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut value = bytes as f64;
    let mut unit = 'K';
    for candidate in ['K', 'M', 'G', 'T'] {
        value /= 1024.0;
        unit = candidate;
        if value < 1024.0 {
            break;
        }
    }
    format!("{value:.1}{unit}")
}

fn meta_time(meta: &std::fs::Metadata) -> SystemTime {
    meta.modified().unwrap_or(std::time::UNIX_EPOCH)
}

pub(super) fn format_time(time: SystemTime, format: &str, utc: bool) -> String {
    let seconds = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `seconds` is valid and `tm` is a local we fully control.
    let ok = unsafe {
        if utc {
            libc::gmtime_r(&seconds, &mut tm)
        } else {
            libc::localtime_r(&seconds, &mut tm)
        }
    };
    if ok.is_null() {
        return String::new();
    }
    let Ok(format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    let mut buffer = [0u8; 64];
    // SAFETY: the buffer and the format are valid for the call.
    let len = unsafe {
        libc::strftime(
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            buffer.len(),
            format.as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&buffer[..len]).into_owned()
}

#[cfg(test)]
mod tests {
    use crate::shell::{ShellState, run};

    fn state() -> (tempfile::TempDir, ShellState) {
        let dir = tempfile::tempdir().unwrap();
        let state = ShellState::new(dir.path().to_path_buf(), "test".to_owned());
        (dir, state)
    }

    #[tokio::test]
    async fn listing_navigation_and_word_counts() {
        let (_dir, mut state) = state();
        let result = run("mkdir sub; echo one > sub/notes.txt; ls", &mut state).await;
        assert_eq!(result.stdout, "sub\n");
        let result = run("cd sub; cat notes.txt; wc -l notes.txt", &mut state).await;
        assert_eq!(result.stdout, "one\n      1 notes.txt\n");
    }

    #[tokio::test]
    async fn head_tail_and_cat_through_pipes() {
        let (_dir, mut state) = state();
        let result = run(
            "printf 'a\\nb\\nc\\nd\\n' > f; head -2 f; echo ---; tail -2 f; echo ---; cat f | head -1",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "a\nb\n---\nc\nd\n---\na\n");
    }

    #[tokio::test]
    async fn copy_move_and_remove() {
        let (_dir, mut state) = state();
        let result = run(
            "mkdir -p a/b; echo x > a/b/f; cp -r a c; cat c/b/f; mv c d; rm -rf a d; ls",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "x\n");
    }

    #[tokio::test]
    async fn find_and_test() {
        let (_dir, mut state) = state();
        let result = run(
            "mkdir d; touch d/x.txt d/y.log; find d -name '*.txt'; test -f d/x.txt && echo file; [ -d d ] && echo dir",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "d/x.txt\nfile\ndir\n");
    }

    #[tokio::test]
    async fn basename_and_dirname_shapes() {
        let (_dir, mut state) = state();
        let result = run(
            "basename /a/b.txt .txt; dirname /a/b.txt; basename plain; dirname plain",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "b\n/a\nplain\n.\n");
    }
}
