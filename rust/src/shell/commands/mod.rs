//! Native command implementations for the in-process shell.
//!
//! Each command is an async function over the shared `Ctx` streams. Error
//! conventions: normal outcomes (including `grep` finding nothing) return
//! `Ok(CommandStatus)`; operational errors return `Err` and the shell prints
//! the message and exits 1, so messages name the command (`cat: ...`).

mod fs;
mod sys;
mod text;

use anyhow::{Result, bail};

use super::exec::{CommandStatus, Ctx, Source};

/// Every supported command name; `docs/commands.md` is kept in sync by a
/// test, and `which` resolves through this list.
pub(crate) const NAMES: &[&str] = &[
    ":", "[", "basename", "cat", "cd", "cp", "cut", "date", "dirname", "du", "echo", "env", "eval",
    "exit", "export", "false", "find", "grep", "head", "hostname", "ls", "mkdir", "mv", "open",
    "printf", "pwd", "readlink", "rm", "rmdir", "sed", "seq", "set", "sh", "sleep", "sort", "stat",
    "tail", "test", "touch", "tr", "true", "uname", "uniq", "unset", "wc", "which", "whoami",
    "yes",
];

pub(crate) fn exists(name: &str) -> bool {
    NAMES.contains(&name)
}

pub(crate) async fn run(name: &str, ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    match name {
        ":" => sys::colon(ctx, args).await,
        "[" => fs::test(ctx, args, true).await,
        "basename" => fs::basename(ctx, args).await,
        "cat" => fs::cat(ctx, args).await,
        "cd" => fs::cd(ctx, args).await,
        "cp" => fs::cp(ctx, args).await,
        "cut" => text::cut(ctx, args).await,
        "date" => sys::date(ctx, args).await,
        "dirname" => fs::dirname(ctx, args).await,
        "du" => fs::du(ctx, args).await,
        "echo" => sys::echo(ctx, args).await,
        "env" => sys::env(ctx, args).await,
        "eval" => sys::eval(ctx, args).await,
        "exit" => sys::exit(ctx, args).await,
        "export" => sys::export(ctx, args).await,
        "false" => sys::false_(ctx, args).await,
        "find" => fs::find(ctx, args).await,
        "grep" => text::grep(ctx, args).await,
        "head" => fs::head(ctx, args).await,
        "hostname" => sys::hostname(ctx, args).await,
        "ls" => fs::ls(ctx, args).await,
        "mkdir" => fs::mkdir(ctx, args).await,
        "mv" => fs::mv(ctx, args).await,
        "open" => sys::open(ctx, args).await,
        "printf" => sys::printf(ctx, args).await,
        "pwd" => fs::pwd(ctx, args).await,
        "readlink" => fs::readlink(ctx, args).await,
        "rm" => fs::rm(ctx, args).await,
        "rmdir" => fs::rmdir(ctx, args).await,
        "sed" => text::sed(ctx, args).await,
        "seq" => sys::seq(ctx, args).await,
        "set" => sys::set(ctx, args).await,
        "sh" => sys::sh(ctx, args).await,
        "sleep" => sys::sleep(ctx, args).await,
        "sort" => text::sort(ctx, args).await,
        "stat" => fs::stat(ctx, args).await,
        "tail" => fs::tail(ctx, args).await,
        "test" => fs::test(ctx, args, false).await,
        "touch" => fs::touch(ctx, args).await,
        "tr" => text::tr(ctx, args).await,
        "true" => sys::true_(ctx, args).await,
        "uname" => sys::uname(ctx, args).await,
        "uniq" => text::uniq(ctx, args).await,
        "unset" => sys::unset(ctx, args).await,
        "wc" => fs::wc(ctx, args).await,
        "which" => sys::which(ctx, args).await,
        "whoami" => sys::whoami(ctx, args).await,
        "yes" => sys::yes(ctx, args).await,
        _ => {
            ctx.write_err(format!("{name}: command not found\n").as_bytes())
                .await?;
            Ok(CommandStatus::code(127))
        }
    }
}

/// Reads all remaining input, up to `limit` bytes.
pub(super) async fn read_all(input: &mut dyn Source, limit: usize, name: &str) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    // Heap so the buffer is not part of every command's future state.
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            return Ok(data);
        }
        if data.len() + count > limit {
            bail!(
                "{name}: input is larger than the {} MiB the shell buffers",
                limit / (1024 * 1024)
            );
        }
        data.extend_from_slice(&buffer[..count]);
    }
}

pub(super) async fn write_line(ctx: &mut Ctx<'_>, text: &str) -> Result<()> {
    let mut line = String::with_capacity(text.len() + 1);
    line.push_str(text);
    line.push('\n');
    ctx.write_out(line.as_bytes()).await
}

/// Shell-style pattern match for one path component (`*`, `?`, `[...]`).
pub(super) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    match_from(&pattern, &text)
}

fn match_from(pattern: &[char], text: &[char]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        '*' => {
            let mut rest = &pattern[1..];
            while rest.first() == Some(&'*') {
                rest = &rest[1..];
            }
            if rest.is_empty() {
                return true;
            }
            (0..=text.len()).any(|skip| match_from(rest, &text[skip..]))
        }
        '?' => !text.is_empty() && match_from(&pattern[1..], &text[1..]),
        '[' => match match_class(pattern, text) {
            Some((matched, consumed)) => {
                matched && !text.is_empty() && match_from(&pattern[consumed..], &text[1..])
            }
            None => !text.is_empty() && text[0] == '[' && match_from(&pattern[1..], &text[1..]),
        },
        character => {
            !text.is_empty() && text[0] == character && match_from(&pattern[1..], &text[1..])
        }
    }
}

/// Parses `[...]` at the start of `pattern`; returns `(matches_char, len)`.
fn match_class(pattern: &[char], text: &[char]) -> Option<(bool, usize)> {
    let close = pattern[1..]
        .iter()
        .position(|&character| character == ']')?
        + 1;
    let character = *text.first()?;
    let mut inner = &pattern[1..close];
    let mut negate = false;
    if inner.first() == Some(&'!') || inner.first() == Some(&'^') {
        negate = true;
        inner = &inner[1..];
    }
    let mut matched = false;
    let mut index = 0;
    while index < inner.len() {
        if index + 2 < inner.len() && inner[index + 1] == '-' && inner[index + 2] != ']' {
            if inner[index] <= character && character <= inner[index + 2] {
                matched = true;
            }
            index += 3;
        } else {
            if inner[index] == character {
                matched = true;
            }
            index += 1;
        }
    }
    Some((matched != negate, close + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_is_documented() {
        let docs = include_str!("../../../../docs/commands.md");
        let mut documented: Vec<String> = Vec::new();
        for line in docs.lines() {
            let cells: Vec<&str> = line.split('|').collect();
            if cells.len() < 3 {
                continue;
            }
            let first = cells[1].trim();
            let mut names = Vec::new();
            let mut parsed = !first.is_empty();
            for part in first.split(" / ") {
                let part = part.trim();
                if part.len() >= 3 && part.starts_with('`') && part.ends_with('`') {
                    names.push(part[1..part.len() - 1].to_string());
                } else {
                    parsed = false;
                    break;
                }
            }
            if parsed {
                documented.extend(names);
            }
        }
        let mut missing: Vec<&str> = NAMES
            .iter()
            .copied()
            .filter(|name| !documented.iter().any(|doc| doc == name))
            .collect();
        missing.sort_unstable();
        assert!(missing.is_empty(), "undocumented commands: {missing:?}");
        let mut unknown: Vec<&str> = documented
            .iter()
            .map(String::as_str)
            .filter(|name| !NAMES.contains(name))
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        assert!(unknown.is_empty(), "documented but unknown: {unknown:?}");
    }

    #[test]
    fn globs_match_the_usual_shapes() {
        assert!(glob_match("*.txt", "notes.txt"));
        assert!(glob_match("*.txt", ".txt"));
        assert!(!glob_match("*.txt", "notes.txt.bak"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("[ab]c", "ac"));
        assert!(!glob_match("[ab]c", "cc"));
        assert!(glob_match("[!a]c", "bc"));
        assert!(glob_match("[a-z]9", "f9"));
        assert!(glob_match("plain", "plain"));
        assert!(!glob_match("plain", "plains"));
    }
}
