//! Text-processing commands: regex grep, a sed subset, sort/uniq/cut/tr.

use anyhow::{Result, bail};
use regex::{Captures, Regex, RegexBuilder};

use super::super::exec::{CommandStatus, Ctx, FileSource};
use super::read_all;

fn read_text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

// ---------------------------------------------------------------- grep

pub(super) async fn grep(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut insensitive = false;
    let mut show_numbers = false;
    let mut invert = false;
    let mut count = false;
    let mut files_only = false;
    let mut recursive = false;
    let mut fixed = false;
    let mut quiet = false;
    let mut pattern: Option<String> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-e" {
            index += 1;
            pattern = Some(
                args.get(index)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("grep: -e requires a pattern"))?,
            );
        } else if arg.starts_with('-') && arg.len() > 1 && pattern.is_none() {
            for letter in arg[1..].chars() {
                match letter {
                    'i' => insensitive = true,
                    'n' => show_numbers = true,
                    'v' => invert = true,
                    'c' => count = true,
                    'l' => files_only = true,
                    'r' | 'R' => recursive = true,
                    'F' => fixed = true,
                    'E' => {}
                    'q' => quiet = true,
                    _ => bail!("grep: invalid option -- '{letter}'"),
                }
            }
        } else if pattern.is_none() {
            pattern = Some(arg.clone());
        } else {
            paths.push(arg.clone());
        }
        index += 1;
    }
    let Some(pattern) = pattern else {
        bail!("grep: missing pattern");
    };
    let expression = if fixed {
        regex::escape(&pattern)
    } else {
        pattern
    };
    let regex = RegexBuilder::new(&expression)
        .case_insensitive(insensitive)
        .build()
        .map_err(|error| anyhow::anyhow!("grep: invalid regular expression: {error}"))?;

    let mut sources: Vec<String> = Vec::new();
    if paths.is_empty() {
        sources.push(String::new()); // empty name = stdin
    } else if recursive {
        for path in &paths {
            collect_files(&ctx.state.cwd, path, &mut sources)?;
        }
    } else {
        sources = paths.clone();
    }
    let multiple = sources.len() > 1;
    let mut found = false;
    let mut errors = false;
    for source in &sources {
        let data = if source.is_empty() {
            match read_all(ctx.input, ctx.state.buffer_limit, "grep").await {
                Ok(data) => data,
                Err(error) => {
                    ctx.write_err(format!("{error}\n").as_bytes()).await?;
                    errors = true;
                    continue;
                }
            }
        } else {
            let path = super::super::exec::resolve(&ctx.state.cwd, source);
            match tokio::fs::File::open(&path).await {
                Ok(handle) => {
                    let mut reader = FileSource::new(handle);
                    match read_all(&mut reader, ctx.state.buffer_limit, "grep").await {
                        Ok(data) => data,
                        Err(error) => {
                            ctx.write_err(format!("{error}\n").as_bytes()).await?;
                            errors = true;
                            continue;
                        }
                    }
                }
                Err(error) => {
                    ctx.write_err(format!("grep: {source}: {error}\n").as_bytes())
                        .await?;
                    errors = true;
                    continue;
                }
            }
        };
        let text = read_text(&data);
        let mut file_matches = 0usize;
        let mut output = String::new();
        let mut listed = false;
        for (number, line) in split_lines(&text).iter().enumerate() {
            let matches = regex.is_match(line) != invert;
            if !matches {
                continue;
            }
            found = true;
            file_matches += 1;
            if quiet || listed {
                continue;
            }
            if files_only {
                listed = true;
                output.push_str(if source.is_empty() {
                    "(standard input)"
                } else {
                    source
                });
                output.push('\n');
                continue;
            }
            if count {
                continue;
            }
            if multiple {
                output.push_str(if source.is_empty() {
                    "(standard input)"
                } else {
                    source
                });
                output.push(':');
            }
            if show_numbers {
                output.push_str(&format!("{}:", number + 1));
            }
            output.push_str(line);
            output.push('\n');
        }
        if count {
            if multiple {
                output.push_str(if source.is_empty() {
                    "(standard input)"
                } else {
                    source
                });
                output.push(':');
            }
            output.push_str(&format!("{file_matches}\n"));
        }
        if !quiet {
            ctx.write_out(output.as_bytes()).await?;
        }
    }
    if errors {
        return Ok(CommandStatus::code(2));
    }
    Ok(CommandStatus::code(if found { 0 } else { 1 }))
}

fn collect_files(cwd: &std::path::Path, path: &str, files: &mut Vec<String>) -> Result<()> {
    let resolved = super::super::exec::resolve(cwd, path);
    let meta = std::fs::symlink_metadata(&resolved)
        .map_err(|error| anyhow::anyhow!("grep: {path}: {error}"))?;
    if meta.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&resolved)
            .map_err(|error| anyhow::anyhow!("grep: {path}: {error}"))?
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if path.ends_with('/') {
                format!("{path}{name}")
            } else {
                format!("{path}/{name}")
            };
            collect_files(cwd, &child, files)?;
        }
    } else {
        files.push(path.to_owned());
    }
    Ok(())
}

// ----------------------------------------------------------------- sed

struct SedCommand {
    first: Option<SedAddress>,
    second: Option<SedAddress>,
    body: SedBody,
}

enum SedAddress {
    Line(usize),
    Last,
    Pattern(Regex),
}

enum SedBody {
    Substitute {
        regex: Regex,
        replacement: String,
        global: bool,
    },
    Print,
    Delete,
    Quit(Option<i32>),
}

pub(super) async fn sed(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut quiet = false;
    let mut in_place = false;
    let mut suffix: Option<String> = None;
    let mut scripts: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut has_e = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-n" {
            quiet = true;
        } else if arg == "-i" {
            in_place = true;
        } else if let Some(rest) = arg.strip_prefix("-i") {
            in_place = true;
            suffix = Some(rest.to_owned());
        } else if arg == "-e" {
            index += 1;
            scripts.push(
                args.get(index)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("sed: -e requires a script"))?,
            );
            has_e = true;
        } else if let Some(rest) = arg.strip_prefix("-e") {
            scripts.push(rest.to_owned());
            has_e = true;
        } else if arg.starts_with('-') && arg.len() > 1 {
            bail!("sed: invalid option '{arg}'");
        } else if !has_e && scripts.is_empty() {
            scripts.push(arg.clone());
        } else {
            files.push(arg.clone());
        }
        index += 1;
    }
    if scripts.is_empty() {
        bail!("sed: missing script");
    }
    let commands =
        parse_sed_script(&scripts.join("\n")).map_err(|error| anyhow::anyhow!("sed: {error}"))?;

    let mut status = 0;
    if files.is_empty() {
        let data = read_all(ctx.input, ctx.state.buffer_limit, "sed").await?;
        let text = read_text(&data);
        let (output, quit) = run_sed(&text, &commands, quiet);
        ctx.write_out(output.as_bytes()).await?;
        return Ok(CommandStatus::code(quit.unwrap_or(0)));
    }
    for file in &files {
        let path = super::super::exec::resolve(&ctx.state.cwd, file);
        let data = match tokio::fs::read(&path).await {
            Ok(data) => data,
            Err(error) => {
                ctx.write_err(format!("sed: {file}: {error}\n").as_bytes())
                    .await?;
                status = 1;
                continue;
            }
        };
        let (output, quit) = run_sed(&read_text(&data), &commands, quiet);
        if in_place {
            if let Some(suffix) = &suffix {
                let backup = format!("{}/{}", path.display(), suffix);
                let _ = std::fs::copy(&path, backup);
            }
            if let Err(error) = tokio::fs::write(&path, output.as_bytes()).await {
                ctx.write_err(format!("sed: {file}: {error}\n").as_bytes())
                    .await?;
                status = 1;
            }
        } else {
            ctx.write_out(output.as_bytes()).await?;
        }
        if let Some(code) = quit {
            return Ok(CommandStatus::code(code));
        }
    }
    Ok(CommandStatus::code(status))
}

fn run_sed(text: &str, commands: &[SedCommand], quiet: bool) -> (String, Option<i32>) {
    let lines = split_lines(text);
    let total = lines.len();
    let mut ranges = vec![false; commands.len()];
    let mut output = String::new();
    let mut quit = None;
    for (position, line) in lines.iter().enumerate() {
        let number = position + 1;
        let mut current = (*line).to_owned();
        let mut deleted = false;
        let mut extra_print = false;
        for (command_index, command) in commands.iter().enumerate() {
            if !sed_selected(command, number, total, &current, &mut ranges[command_index]) {
                continue;
            }
            match &command.body {
                SedBody::Substitute {
                    regex,
                    replacement,
                    global,
                } => {
                    current = apply_substitute(&current, regex, replacement, *global);
                }
                SedBody::Print => extra_print = true,
                SedBody::Delete => deleted = true,
                SedBody::Quit(code) => {
                    quit = Some(code.unwrap_or(0));
                }
            }
        }
        if !quiet && !deleted {
            output.push_str(&current);
            output.push('\n');
        }
        if extra_print {
            output.push_str(&current);
            output.push('\n');
        }
        if quit.is_some() {
            break;
        }
    }
    (output, quit)
}

fn sed_selected(
    command: &SedCommand,
    number: usize,
    total: usize,
    line: &str,
    active: &mut bool,
) -> bool {
    if *active {
        // Inside a range: the line is selected; the range ends on the
        // second address (inclusive).
        if let Some(second) = &command.second
            && sed_address_matches(second, number, total, line)
        {
            *active = false;
        }
        return true;
    }
    let Some(first) = &command.first else {
        return true;
    };
    if !sed_address_matches(first, number, total, line) {
        return false;
    }
    if let Some(second) = &command.second {
        *active = true;
        if sed_address_matches(second, number, total, line) {
            *active = false;
        }
    }
    true
}

fn sed_address_matches(address: &SedAddress, number: usize, total: usize, line: &str) -> bool {
    match address {
        SedAddress::Line(target) => *target == number,
        SedAddress::Last => number == total,
        SedAddress::Pattern(regex) => regex.is_match(line),
    }
}

fn apply_substitute(line: &str, regex: &Regex, replacement: &str, global: bool) -> String {
    let fix = |captures: &Captures<'_>| expand_replacement(replacement, captures);
    if global {
        regex.replace_all(line, fix).into_owned()
    } else {
        regex.replace(line, fix).into_owned()
    }
}

fn expand_replacement(template: &str, captures: &Captures<'_>) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '&' => {
                if let Some(matched) = captures.get(0) {
                    out.push_str(matched.as_str());
                }
            }
            '\\' => match chars.next() {
                Some(digit @ '0'..='9') => {
                    let index = digit as usize - '0' as usize;
                    if let Some(captured) = captures.get(index) {
                        out.push_str(captured.as_str());
                    }
                }
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            other => out.push(other),
        }
    }
    out
}

fn parse_sed_script(script: &str) -> Result<Vec<SedCommand>, String> {
    let chars: Vec<char> = script.chars().collect();
    let mut commands = Vec::new();
    let mut index = 0;
    loop {
        while index < chars.len() && matches!(chars[index], ' ' | '\t' | '\n' | ';') {
            index += 1;
        }
        if index >= chars.len() {
            return Ok(commands);
        }
        let first = parse_sed_address(&chars, &mut index)?;
        let mut second = None;
        if index < chars.len() && chars[index] == ',' {
            index += 1;
            second = Some(match parse_sed_address(&chars, &mut index)? {
                Some(address) => address,
                None => return Err("expected an address after ','".to_owned()),
            });
        }
        if index >= chars.len() {
            return Err("expected a command".to_owned());
        }
        let command_character = chars[index];
        index += 1;
        let body = match command_character {
            's' => {
                let Some(&delimiter) = chars.get(index) else {
                    return Err("expected a delimiter after 's'".to_owned());
                };
                index += 1;
                let pattern = read_until_delimiter(&chars, &mut index, delimiter)?;
                let replacement = read_until_delimiter(&chars, &mut index, delimiter)?;
                let mut global = false;
                let mut insensitive = false;
                while index < chars.len() && !matches!(chars[index], ';' | '\n' | ' ') {
                    match chars[index] {
                        'g' => global = true,
                        'i' => insensitive = true,
                        other => return Err(format!("unsupported 's' flag '{other}'")),
                    }
                    index += 1;
                }
                let regex = RegexBuilder::new(&pattern)
                    .case_insensitive(insensitive)
                    .build()
                    .map_err(|error| format!("invalid regular expression: {error}"))?;
                SedBody::Substitute {
                    regex,
                    replacement,
                    global,
                }
            }
            'p' => SedBody::Print,
            'd' => SedBody::Delete,
            'q' => {
                let mut digits = String::new();
                while index < chars.len() && chars[index].is_ascii_digit() {
                    digits.push(chars[index]);
                    index += 1;
                }
                let code = if digits.is_empty() {
                    None
                } else {
                    Some(digits.parse().map_err(|_| "invalid 'q' code".to_owned())?)
                };
                SedBody::Quit(code)
            }
            other => return Err(format!("unsupported command '{other}'")),
        };
        commands.push(SedCommand {
            first,
            second,
            body,
        });
    }
}

fn parse_sed_address(chars: &[char], index: &mut usize) -> Result<Option<SedAddress>, String> {
    if *index >= chars.len() {
        return Ok(None);
    }
    match chars[*index] {
        '0'..='9' => {
            let mut digits = String::new();
            while *index < chars.len() && chars[*index].is_ascii_digit() {
                digits.push(chars[*index]);
                *index += 1;
            }
            let line = digits
                .parse()
                .map_err(|_| format!("invalid line address '{digits}'"))?;
            Ok(Some(SedAddress::Line(line)))
        }
        '$' => {
            *index += 1;
            Ok(Some(SedAddress::Last))
        }
        '/' => {
            *index += 1;
            let pattern = read_until_delimiter(chars, index, '/')?;
            let regex = Regex::new(&pattern)
                .map_err(|error| format!("invalid regular expression: {error}"))?;
            Ok(Some(SedAddress::Pattern(regex)))
        }
        _ => Ok(None),
    }
}

fn read_until_delimiter(
    chars: &[char],
    index: &mut usize,
    delimiter: char,
) -> Result<String, String> {
    let mut out = String::new();
    while *index < chars.len() {
        let character = chars[*index];
        if character == '\\' && *index + 1 < chars.len() {
            out.push(character);
            out.push(chars[*index + 1]);
            *index += 2;
            continue;
        }
        if character == delimiter {
            *index += 1;
            return Ok(out);
        }
        out.push(character);
        *index += 1;
    }
    Err(format!("unterminated '{delimiter}' section"))
}

// ------------------------------------------------------- sort, uniq, cut

pub(super) async fn sort(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut numeric = false;
    let mut reverse = false;
    let mut unique = false;
    let mut files = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    'n' => numeric = true,
                    'r' => reverse = true,
                    'u' => unique = true,
                    _ => bail!("sort: invalid option -- '{letter}'"),
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
    let data = read_inputs(ctx, &files, "sort").await?;
    let text = read_text(&data);
    let mut lines: Vec<String> = split_lines(&text)
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
    if numeric {
        lines.sort_by(|left, right| {
            numeric_key(left)
                .total_cmp(&numeric_key(right))
                .then_with(|| left.cmp(right))
        });
    } else {
        lines.sort();
    }
    if reverse {
        lines.reverse();
    }
    if unique {
        lines.dedup();
    }
    let mut output = String::new();
    for line in &lines {
        output.push_str(line);
        output.push('\n');
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

fn numeric_key(line: &str) -> f64 {
    let trimmed = line.trim_start();
    let candidate: String = trimmed
        .chars()
        .take_while(|character| {
            character.is_ascii_digit() || matches!(character, '+' | '-' | '.' | 'e' | 'E')
        })
        .collect();
    candidate.parse().unwrap_or(0.0)
}

async fn read_inputs(ctx: &mut Ctx<'_>, files: &[String], name: &str) -> Result<Vec<u8>> {
    if files.is_empty() {
        return read_all(ctx.input, ctx.state.buffer_limit, name).await;
    }
    let mut data = Vec::new();
    for file in files {
        let path = super::super::exec::resolve(&ctx.state.cwd, file);
        let chunk = tokio::fs::read(&path)
            .await
            .map_err(|error| anyhow::anyhow!("{name}: {file}: {error}"))?;
        if data.len() + chunk.len() > ctx.state.buffer_limit {
            bail!("{name}: input is larger than the buffer limit");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

pub(super) async fn uniq(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut count = false;
    let mut duplicates_only = false;
    let mut unique_only = false;
    let mut files = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            for letter in arg[1..].chars() {
                match letter {
                    'c' => count = true,
                    'd' => duplicates_only = true,
                    'u' => unique_only = true,
                    _ => bail!("uniq: invalid option -- '{letter}'"),
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
    if files.len() > 1 {
        bail!("uniq: extra operand '{}'", files[1]);
    }
    let data = read_inputs(ctx, &files, "uniq").await?;
    let text = read_text(&data);
    let lines = split_lines(&text);
    let mut output = String::new();
    let mut index = 0;
    while index < lines.len() {
        let mut end = index + 1;
        while end < lines.len() && lines[end] == lines[index] {
            end += 1;
        }
        let occurrences = end - index;
        let wanted = if duplicates_only {
            occurrences > 1
        } else if unique_only {
            occurrences == 1
        } else {
            true
        };
        if wanted {
            if count {
                output.push_str(&format!("{:>7} {}\n", occurrences, lines[index]));
            } else {
                output.push_str(lines[index]);
                output.push('\n');
            }
        }
        index = end;
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

pub(super) async fn cut(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut delimiter = '\t';
    let mut fields: Option<String> = None;
    let mut characters: Option<String> = None;
    let mut suppress = false;
    let mut files = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-d" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("cut: -d requires a delimiter"))?;
                delimiter = value
                    .chars()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("cut: empty delimiter"))?;
            }
            "-f" => {
                index += 1;
                fields = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("cut: -f requires a list"))?,
                );
            }
            "-c" => {
                index += 1;
                characters = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("cut: -c requires a list"))?,
                );
            }
            "-s" => suppress = true,
            other if other.starts_with('-') && other.len() > 1 => {
                bail!("cut: invalid option '{other}'");
            }
            _ => files.push(arg.clone()),
        }
        index += 1;
    }
    if fields.is_some() == characters.is_some() {
        bail!("cut: exactly one of -f and -c is required");
    }
    let data = read_inputs(ctx, &files, "cut").await?;
    let text = read_text(&data);
    let mut output = String::new();
    for line in split_lines(&text) {
        if let Some(spec) = &fields {
            if !line.contains(delimiter) {
                if !suppress {
                    output.push_str(line);
                    output.push('\n');
                }
                continue;
            }
            let parts: Vec<&str> = line.split(delimiter).collect();
            let mut selected: Vec<&str> = Vec::new();
            for (start, end) in parse_cut_list(spec)? {
                for index in start..=end.min(parts.len()) {
                    if index >= 1 && index <= parts.len() {
                        selected.push(parts[index - 1]);
                    }
                }
            }
            output.push_str(&selected.join(&delimiter.to_string()));
            output.push('\n');
        } else if let Some(spec) = &characters {
            let chars: Vec<char> = line.chars().collect();
            let mut selected = String::new();
            for (start, end) in parse_cut_list(spec)? {
                for index in start..=end.min(chars.len()) {
                    if index >= 1 && index <= chars.len() {
                        selected.push(chars[index - 1]);
                    }
                }
            }
            output.push_str(&selected);
            output.push('\n');
        }
    }
    ctx.write_out(output.as_bytes()).await?;
    Ok(CommandStatus::code(0))
}

fn parse_cut_list(spec: &str) -> Result<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: usize = if start.is_empty() {
                1
            } else {
                start
                    .parse()
                    .map_err(|_| anyhow::anyhow!("cut: invalid range '{part}'"))?
            };
            let end: usize = if end.is_empty() {
                usize::MAX
            } else {
                end.parse()
                    .map_err(|_| anyhow::anyhow!("cut: invalid range '{part}'"))?
            };
            ranges.push((start, end));
        } else {
            let value: usize = part
                .parse()
                .map_err(|_| anyhow::anyhow!("cut: invalid range '{part}'"))?;
            ranges.push((value, value));
        }
    }
    Ok(ranges)
}

// ------------------------------------------------------------------- tr

pub(super) async fn tr(ctx: &mut Ctx<'_>, args: &[String]) -> Result<CommandStatus> {
    let mut delete = false;
    let mut sets = Vec::new();
    for arg in args {
        if arg == "-d" {
            delete = true;
        } else if arg.starts_with('-') && arg.len() > 1 {
            bail!("tr: invalid option '{arg}'");
        } else {
            sets.push(arg.clone());
        }
    }
    let (set1, set2) = match sets.len() {
        1 => (expand_set(&sets[0]), None),
        2 => (expand_set(&sets[0]), Some(expand_set(&sets[1]))),
        _ => bail!("tr: expected one or two sets"),
    };
    if delete {
        let data = read_all(ctx.input, ctx.state.buffer_limit, "tr").await?;
        let filtered: Vec<u8> = data
            .iter()
            .copied()
            .filter(|byte| !set1.contains(byte))
            .collect();
        ctx.write_out(&filtered).await?;
        return Ok(CommandStatus::code(0));
    }
    let Some(set2) = set2 else {
        bail!("tr: translation needs two sets (or use -d)");
    };
    let data = read_all(ctx.input, ctx.state.buffer_limit, "tr").await?;
    let translated: Vec<u8> = data
        .iter()
        .map(
            |byte| match set1.iter().position(|candidate| candidate == byte) {
                Some(position) => set2[position.min(set2.len() - 1)],
                None => *byte,
            },
        )
        .collect();
    ctx.write_out(&translated).await?;
    Ok(CommandStatus::code(0))
}

fn expand_set(spec: &str) -> Vec<u8> {
    let bytes: Vec<u8> = spec.bytes().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if index + 2 < bytes.len() && bytes[index + 1] == b'-' && bytes[index + 2] != b'-' {
            for value in bytes[index]..=bytes[index + 2] {
                out.push(value);
            }
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
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
    async fn grep_filters_and_numbers() {
        let (_dir, mut state) = state();
        let result = run(
            "printf 'alpha\\nbeta\\nGamma\\n' > f; grep -i gamma f; grep -n beta f; grep -c a f; grep zzz f; echo $?",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "Gamma\n2:beta\n3\n1\n");
    }

    #[tokio::test]
    async fn sed_substitutes_and_prints_ranges() {
        let (_dir, mut state) = state();
        let result = run(
            "printf 'one\\ntwo\\nthree\\n' > f; sed 's/o/0/' f; echo ---; sed -n '2,3p' f",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "0ne\ntw0\nthree\n---\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn sed_can_edit_in_place() {
        let (_dir, mut state) = state();
        let result = run("echo hello > f; sed -i 's/hello/hi/' f; cat f", &mut state).await;
        assert_eq!(result.stdout, "hi\n");
    }

    #[tokio::test]
    async fn sort_uniq_cut_and_tr() {
        let (_dir, mut state) = state();
        let result = run(
            "printf 'b\\na\\nb\\n' | sort | uniq -c; printf '3 c\\n1 a\\n2 b\\n' | sort -n | cut -d ' ' -f 2; echo abc | tr a-z A-Z",
            &mut state,
        )
        .await;
        assert_eq!(result.stdout, "      1 a\n      2 b\na\nb\nc\nABC\n");
    }
}
