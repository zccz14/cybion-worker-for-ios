//! Shell builtins and system commands: variables, output, time, identity.

use anyhow::{Result, bail};

use super::super::exec::{CommandStatus, Ctx};
use super::{exists, read_all, write_line};
use crate::shell::run_with_status;

// ------------------------------------------------------------ builtins

pub(super) async fn colon(_ctx: &mut Ctx<'_>, _args: &[String]) -> Result<CommandStatus> {
    Ok(CommandStatus::code(0))
}

pub(super) async fn true_(_ctx: &mut Ctx<'_>, _args: &[String]) -> Result<CommandStatus> {
    Ok(CommandStatus::code(0))
}

pub(super) async fn false_(_ctx: &mut Ctx<'_>, _args: &[String]) -> Result<CommandStatus> {
    Ok(CommandStatus::code(1))
}

pub(super) async fn exit(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let code = match args.len() {
        0 => ctx.state.last_status,
        1 => args[0]
            .parse()
            .map_err(|_| anyhow::anyhow!("exit: {}: numeric argument required", args[0]))?,
        _ => bail!("exit: too many arguments"),
    };
    Ok(CommandStatus::exit(code))
}

pub(super) async fn echo(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut newline = true;
    let mut escapes = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let is_flags = arg.len() > 1
            && arg.starts_with('-')
            && arg[1..]
                .chars()
                .all(|letter| matches!(letter, 'n' | 'e' | 'E'));
        if !is_flags {
            break;
        }
        for letter in arg[1..].chars() {
            match letter {
                'n' => newline = false,
                'e' => escapes = true,
                'E' => escapes = false,
                _ => {}
            }
        }
        index += 1;
    }
    let joined = args[index..].join(" ");
    let mut output = if escapes { unescape(&joined) } else { joined };
    if newline {
        output.push('\n');
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn printf(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let Some(format) = args.first() else {
        bail!("printf: missing format");
    };
    let mut output = String::new();
    let mut next_argument = 1;
    loop {
        let before = next_argument;
        output.push_str(&render_format(format, args, &mut next_argument)?);
        if next_argument >= args.len() || next_argument == before {
            break;
        }
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

fn render_format(format: &str, args: &[String], next_argument: &mut usize) -> Result<String> {
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' => push_escape(&mut chars, &mut out),
            '%' => {
                let mut left = false;
                let mut zero = false;
                while let Some(flag) = chars.peek().copied() {
                    match flag {
                        '-' => left = true,
                        '0' => zero = true,
                        '+' | ' ' | '#' => {}
                        _ => break,
                    }
                    chars.next();
                }
                let mut width = 0usize;
                while let Some(digit) = chars.next_if(|digit| digit.is_ascii_digit()) {
                    width = width * 10 + digit.to_digit(10).unwrap_or(0) as usize;
                }
                let mut precision: Option<usize> = None;
                if chars.peek() == Some(&'.') {
                    chars.next();
                    let mut value = 0usize;
                    while let Some(digit) = chars.next_if(|digit| digit.is_ascii_digit()) {
                        value = value * 10 + digit.to_digit(10).unwrap_or(0) as usize;
                    }
                    precision = Some(value);
                }
                let Some(conversion) = chars.next() else {
                    bail!("printf: the format ends with '%'");
                };
                if conversion == '%' {
                    out.push('%');
                    continue;
                }
                let argument = args.get(*next_argument).map(String::as_str).unwrap_or("");
                if *next_argument < args.len() {
                    *next_argument += 1;
                }
                match conversion {
                    's' => {
                        let text = match precision {
                            Some(limit) => argument.chars().take(limit).collect::<String>(),
                            None => argument.to_owned(),
                        };
                        push_padded(&mut out, &text, width, left, false);
                    }
                    'c' => {
                        let text = argument
                            .chars()
                            .next()
                            .map(String::from)
                            .unwrap_or_default();
                        push_padded(&mut out, &text, width, left, false);
                    }
                    'd' | 'i' => {
                        let value: i64 = argument.trim().parse().unwrap_or(0);
                        push_padded(&mut out, &value.to_string(), width, left, zero);
                    }
                    'u' => {
                        let value: u64 = argument.trim().parse().unwrap_or(0);
                        push_padded(&mut out, &value.to_string(), width, left, zero);
                    }
                    'x' => {
                        let value: u64 = argument.trim().parse().unwrap_or(0);
                        push_padded(&mut out, &format!("{value:x}"), width, left, zero);
                    }
                    'X' => {
                        let value: u64 = argument.trim().parse().unwrap_or(0);
                        push_padded(&mut out, &format!("{value:X}"), width, left, zero);
                    }
                    'o' => {
                        let value: u64 = argument.trim().parse().unwrap_or(0);
                        push_padded(&mut out, &format!("{value:o}"), width, left, zero);
                    }
                    'f' => {
                        let value: f64 = argument.trim().parse().unwrap_or(0.0);
                        let text = format!("{:.*}", precision.unwrap_or(6), value);
                        push_padded(&mut out, &text, width, left, zero);
                    }
                    other => bail!("printf: invalid directive '%{other}'"),
                }
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn push_padded(out: &mut String, text: &str, width: usize, left: bool, zero: bool) {
    if text.len() >= width {
        out.push_str(text);
        return;
    }
    let padding = width - text.len();
    if left {
        out.push_str(text);
        out.extend(std::iter::repeat_n(' ', padding));
    } else if zero && !text.starts_with('-') {
        out.extend(std::iter::repeat_n('0', padding));
        out.push_str(text);
    } else if zero {
        out.push('-');
        out.extend(std::iter::repeat_n('0', padding));
        out.push_str(&text[1..]);
    } else {
        out.extend(std::iter::repeat_n(' ', padding));
        out.push_str(text);
    }
}

fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\\' {
            push_escape(&mut chars, &mut out);
        } else {
            out.push(character);
        }
    }
    out
}

fn push_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String) {
    let Some(character) = chars.next() else {
        out.push('\\');
        return;
    };
    match character {
        'n' => out.push('\n'),
        't' => out.push('\t'),
        'r' => out.push('\r'),
        'a' => out.push('\x07'),
        'b' => out.push('\x08'),
        'f' => out.push('\x0c'),
        'v' => out.push('\x0b'),
        '\\' => out.push('\\'),
        '0'..='7' => {
            let mut value = character as u32 - '0' as u32;
            let mut digits = 1;
            while digits < 3 {
                match chars.peek().copied() {
                    Some(next @ '0'..='7') => {
                        value = value * 8 + (next as u32 - '0' as u32);
                        chars.next();
                        digits += 1;
                    }
                    _ => break,
                }
            }
            if let Some(character) = char::from_u32(value) {
                out.push(character);
            }
        }
        'x' => {
            let mut value = 0u32;
            let mut digits = 0;
            while digits < 2 {
                match chars.peek().copied().and_then(|next| next.to_digit(16)) {
                    Some(digit) => {
                        value = value * 16 + digit;
                        chars.next();
                        digits += 1;
                    }
                    None => break,
                }
            }
            if digits == 0 {
                out.push('x');
            } else if let Some(character) = char::from_u32(value) {
                out.push(character);
            }
        }
        other => {
            out.push('\\');
            out.push(other);
        }
    }
}

// ----------------------------------------------------------- variables

pub(super) async fn env(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut output = String::new();
    if args.is_empty() {
        for (name, value) in &ctx.state.vars {
            output.push_str(&format!("{name}={value}\n"));
        }
    } else {
        for name in args {
            if name.contains('=') {
                bail!(
                    "env: running commands with a modified environment is not supported (use NAME=value cmd)"
                );
            }
            output.push_str(ctx.state.var(name).unwrap_or(""));
            output.push('\n');
        }
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn export(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let assignments: Vec<&String> = args.iter().filter(|arg| arg.as_str() != "-p").collect();
    if assignments.is_empty() {
        let mut output = String::new();
        for (name, value) in &ctx.state.vars {
            output.push_str(&format!("export {name}={value}\n"));
        }
        ctx.write_out(output.as_bytes()).await?;
        return Ok(CommandStatus::code(0));
    }
    for assignment in assignments {
        if let Some((name, value)) = assignment.split_once('=') {
            if !super::super::expand::valid_name(name) {
                bail!("export: bad identifier '{name}'");
            }
            ctx.state.vars.insert(name.to_owned(), value.to_owned());
        } else if !super::super::expand::valid_name(assignment) {
            bail!("export: bad identifier '{assignment}'");
        }
    }
    Ok(CommandStatus::code(0))
}

pub(super) async fn unset(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    for name in args {
        if name.starts_with('-') && name.len() > 1 {
            bail!("unset: invalid option '{name}'");
        }
        ctx.state.vars.remove(name);
    }
    Ok(CommandStatus::code(0))
}

pub(super) async fn set(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    if !args.is_empty() {
        bail!("set: this in-process shell has no set options");
    }
    let mut output = String::new();
    for (name, value) in &ctx.state.vars {
        output.push_str(&format!("{name}={value}\n"));
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

// ------------------------------------------------------ running scripts

async fn run_script(ctx: &mut Ctx<'_>, script: &str) -> Result<CommandStatus> {
    // Boxed: the command/execution cycle needs a break in the future type.
    let result = Box::pin(run_with_status(script, &mut *ctx.state)).await;
    if !result.stdout.is_empty() {
        ctx.write_out(result.stdout.as_bytes()).await?;
    }
    if !result.stderr.is_empty() {
        ctx.write_err(result.stderr.as_bytes()).await?;
    }
    Ok(CommandStatus::code(result.exit_code))
}

pub(super) async fn eval(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let script = args.join(" ");
    run_script(ctx, &script).await
}

pub(super) async fn sh(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let script = match args.first().map(String::as_str) {
        Some("-c") => args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("sh: -c requires a command"))?,
        Some(other) => bail!("sh: unexpected argument '{other}' (only '-c' is supported)"),
        None => {
            let data = read_all(&mut *ctx.input, ctx.state.buffer_limit, "sh").await?;
            String::from_utf8_lossy(&data).into_owned()
        }
    };
    run_script(ctx, &script).await
}

pub(super) async fn which(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    if args.is_empty() {
        bail!("which: missing argument");
    }
    let mut missing = false;
    let mut output = String::new();
    for name in args {
        if exists(name) {
            output.push_str(name);
            output.push('\n');
        } else {
            missing = true;
        }
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(if missing { 1 } else { 0 }))
}

// ------------------------------------------------------ system commands

pub(super) async fn sleep(_ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let Some(argument) = args.first() else {
        bail!("sleep: missing operand");
    };
    let seconds = parse_duration(argument)?;
    tokio::time::sleep(std::time::Duration::from_secs_f64(seconds)).await;
    Ok(CommandStatus::code(0))
}

fn parse_duration(text: &str) -> Result<f64> {
    let (number, multiplier) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1.0),
        Some('m') => (&text[..text.len() - 1], 60.0),
        Some('h') => (&text[..text.len() - 1], 3600.0),
        Some('d') => (&text[..text.len() - 1], 86400.0),
        _ => (text, 1.0),
    };
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("sleep: invalid time interval '{text}'"))?;
    if value < 0.0 || !value.is_finite() {
        bail!("sleep: invalid time interval '{text}'");
    }
    Ok(value * multiplier)
}

pub(super) async fn seq(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let (first, step, last) = match args.len() {
        1 => (1.0, 1.0, parse_number(&args[0])?),
        2 => (parse_number(&args[0])?, 1.0, parse_number(&args[1])?),
        3 => (
            parse_number(&args[0])?,
            parse_number(&args[1])?,
            parse_number(&args[2])?,
        ),
        _ => bail!("seq: expected one to three numbers"),
    };
    if step == 0.0 {
        bail!("seq: step cannot be zero");
    }
    let count = ((last - first) / step).floor() + 1.0;
    if count > 1_000_000.0 {
        bail!("seq: too many numbers");
    }
    let integers = args.iter().all(|arg| arg.parse::<i64>().is_ok());
    let mut output = String::new();
    let mut index = 0i64;
    while (index as f64) < count.max(0.0) {
        let value = first + step * index as f64;
        if integers {
            output.push_str(&format!("{}", value as i64));
        } else {
            output.push_str(&format!("{value}"));
        }
        output.push('\n');
        index += 1;
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

fn parse_number(text: &str) -> Result<f64> {
    text.parse()
        .map_err(|_| anyhow::anyhow!("seq: invalid number '{text}'"))
}

pub(super) async fn yes(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let text = if args.is_empty() {
        "y".to_owned()
    } else {
        args.join(" ")
    };
    let mut line = text;
    line.push('\n');
    let mut written = 0u64;
    loop {
        // Stops with a broken-pipe error when the consumer exits.
        ctx.write_out(line.as_bytes()).await?;
        written += 1;
        if written.is_multiple_of(1024) {
            tokio::task::yield_now().await;
        }
    }
}

pub(super) async fn date(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut utc = false;
    let mut format: Option<String> = None;
    for arg in args {
        if arg == "-u" {
            utc = true;
        } else if let Some(rest) = arg.strip_prefix('+') {
            format = Some(rest.to_owned());
        } else {
            bail!("date: unsupported argument '{arg}'");
        }
    }
    let now = std::time::SystemTime::now();
    let text = match format {
        Some(format) if format.contains("%s") => {
            let seconds = now
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let replaced = format.replace("%s", &seconds.to_string());
            super::fs::format_time(now, &replaced, utc)
        }
        Some(format) => super::fs::format_time(now, &format, utc),
        None => super::fs::format_time(now, "%a %b %e %H:%M:%S %Z %Y", utc),
    };
    write_line(ctx, &text).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn uname(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut all = false;
    let mut letters: Vec<char> = Vec::new();
    for arg in args {
        if arg == "-a" {
            all = true;
        } else if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    's' | 'n' | 'r' | 'v' | 'm' => letters.push(letter),
                    _ => bail!("uname: invalid option -- '{letter}'"),
                }
            }
        } else {
            bail!("uname: unexpected argument '{arg}'");
        }
    }
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `uname` fills the struct we pass to it.
    if unsafe { libc::uname(&mut name) } != 0 {
        bail!("uname: {}", std::io::Error::last_os_error());
    }
    let field = |bytes: &[libc::c_char]| -> String {
        let bytes: Vec<u8> = bytes
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let (sysname, nodename, release, version, machine) = (
        field(&name.sysname),
        field(&name.nodename),
        field(&name.release),
        field(&name.version),
        field(&name.machine),
    );
    let text = if all {
        format!("{sysname} {nodename} {release} {version} {machine}")
    } else if letters.is_empty() {
        sysname
    } else {
        let parts: Vec<String> = letters
            .iter()
            .map(|letter| match letter {
                's' => sysname.clone(),
                'n' => nodename.clone(),
                'r' => release.clone(),
                'v' => version.clone(),
                'm' => machine.clone(),
                _ => String::new(),
            })
            .collect();
        parts.join(" ")
    };
    write_line(ctx, &text).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn hostname(ctx: &mut Ctx<'_>, _args: &[String]) -> Result<CommandStatus> {
    let name = ctx.state.hostname.clone();
    write_line(ctx, &name).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn whoami(ctx: &mut Ctx<'_>, _args: &[String]) -> Result<CommandStatus> {
    write_line(ctx, "mobile").await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn open(_ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let Some(url) = args.first() else {
        bail!("open: missing URL");
    };
    match crate::platform::open_url(url) {
        Ok(true) => Ok(CommandStatus::code(0)),
        Ok(false) => bail!("open: the app refused to open '{url}'"),
        Err(error) => Err(error.context("open")),
    }
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
    async fn echo_printf_and_escapes() {
        let (_dir, mut state) = state();
        let result = run(
            "echo -n hello; echo ' world'; echo -e 'a\\tb'; printf '%s-%d|' x 7; printf '\\n'",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "hello world\na\tb\nx-7|\n");
    }

    #[tokio::test]
    async fn variables_export_and_unset() {
        let (_dir, mut state) = state();
        let result = run("A=1; export B=2; env B; unset A; echo [$A]", &mut state).await;
        assert_eq!(result.stdout, "2\n[]\n");
    }

    #[tokio::test]
    async fn seq_which_and_exit_status() {
        let (_dir, mut state) = state();
        let result = run("seq 1 3; which cat; which nope; echo $?", &mut state).await;
        assert_eq!(result.stdout, "1\n2\n3\ncat\n1\n");
    }

    #[tokio::test]
    async fn yes_stops_when_the_consumer_exits() {
        let (_dir, mut state) = state();
        let result = run("yes hello | head -2", &mut state).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "hello\nhello\n");
    }

    #[tokio::test]
    async fn sh_and_eval_run_scripts_in_place() {
        let (_dir, mut state) = state();
        let result = run("sh -c 'echo inner'; eval 'echo evaled'", &mut state).await;
        assert_eq!(result.stdout, "inner\nevaled\n");
    }

    #[tokio::test]
    async fn date_and_uname_look_sane() {
        let (_dir, mut state) = state();
        let result = run("date +%Y; uname", &mut state).await;
        let lines: Vec<&str> = result.stdout.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 4);
        assert_eq!(lines[1], "Darwin");
    }
}
