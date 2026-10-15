//! Word expansion: quotes, escapes, `$VAR` / `${VAR}` / `$?` / `$$`,
//! `$(...)` command substitution and leading `~`.
//!
//! Deliberately absent (each fails or stays literal, see docs/commands.md):
//! field splitting, pathname expansion (globbing), arithmetic `$((...))`,
//! brace forms like `${VAR:-x}` and `~user`.

use anyhow::{Result, bail};

use super::ShellState;
use super::exec::Sink;

/// Expands one raw word (quotes still attached) into its final value.
///
/// Returns `None` when the word vanishes: an unquoted word whose expansions
/// produced nothing and that had no quoted part at all. POSIX removes such
/// words; `ls $UNSET` therefore means `ls`, not `ls ''`.
pub(crate) async fn expand_word(
    raw: &str,
    state: &ShellState,
    err: &mut dyn Sink,
) -> Result<Option<String>> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut quoted = false;
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\'' => {
                quoted = true;
                index += 1;
                while index < chars.len() && chars[index] != '\'' {
                    out.push(chars[index]);
                    index += 1;
                }
                if index == chars.len() {
                    bail!("unterminated single quote");
                }
                index += 1;
            }
            '"' => {
                quoted = true;
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    match chars[index] {
                        '\\' if index + 1 < chars.len()
                            && matches!(chars[index + 1], '\\' | '"' | '$') =>
                        {
                            out.push(chars[index + 1]);
                            index += 2;
                        }
                        '\\' if index + 1 < chars.len() && chars[index + 1] == '\n' => {
                            index += 2;
                        }
                        '$' => {
                            index = expand_dollar(&chars, index, state, err, &mut out).await?;
                        }
                        other => {
                            out.push(other);
                            index += 1;
                        }
                    }
                }
                if index == chars.len() {
                    bail!("unterminated double quote");
                }
                index += 1;
            }
            '\\' => {
                if index + 1 < chars.len() {
                    if chars[index + 1] == '\n' {
                        index += 2;
                    } else {
                        out.push(chars[index + 1]);
                        index += 2;
                    }
                } else {
                    index += 1;
                }
            }
            '$' => {
                index = expand_dollar(&chars, index, state, err, &mut out).await?;
            }
            '~' if index == 0 && (chars.len() == 1 || chars[1] == '/') => {
                out.push_str(state.var("HOME").unwrap_or("~"));
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    if out.is_empty() && !quoted {
        return Ok(None);
    }
    Ok(Some(out))
}

/// Handles a `$` starting at `index`; returns the index after it.
async fn expand_dollar(
    chars: &[char],
    index: usize,
    state: &ShellState,
    err: &mut dyn Sink,
    out: &mut String,
) -> Result<usize> {
    let next = index + 1;
    let Some(&character) = chars.get(next) else {
        out.push('$');
        return Ok(next);
    };
    match character {
        '?' => {
            out.push_str(&state.last_status.to_string());
            Ok(next + 1)
        }
        '$' => {
            out.push_str(&std::process::id().to_string());
            Ok(next + 1)
        }
        '{' => {
            let mut end = next + 1;
            while end < chars.len() && chars[end] != '}' {
                end += 1;
            }
            if end == chars.len() {
                bail!("unterminated parameter expansion: ${{...}}");
            }
            let name: String = chars[next + 1..end].iter().collect();
            if !valid_name(&name) {
                bail!("unsupported parameter expansion: ${{{name}}}");
            }
            out.push_str(state.var(&name).unwrap_or(""));
            Ok(end + 1)
        }
        '(' => {
            if chars.get(next + 1) == Some(&'(') {
                bail!("arithmetic expansion $((...)) is not supported");
            }
            let (script, after) = scan_command_substitution(chars, next)?;
            // A substitution runs in a subshell: mutations stay local.
            let mut sub = state.clone();
            // Boxed: the expansion/execution cycle needs a break in the type.
            let result = Box::pin(super::run_with_status(&script, &mut sub)).await;
            if !result.stderr.is_empty() {
                err.write(result.stderr.as_bytes()).await?;
            }
            out.push_str(result.stdout.trim_end_matches('\n'));
            Ok(after)
        }
        character if character == '_' || character.is_ascii_alphabetic() => {
            let mut end = next + 1;
            while end < chars.len() && (chars[end] == '_' || chars[end].is_ascii_alphanumeric()) {
                end += 1;
            }
            let name: String = chars[next..end].iter().collect();
            out.push_str(state.var(&name).unwrap_or(""));
            Ok(end)
        }
        _ => {
            out.push('$');
            Ok(next)
        }
    }
}

/// Scans `$(...)` starting at the `(`; returns the script and the index after
/// the closing parenthesis. Tracks nesting and quotes inside the script.
fn scan_command_substitution(chars: &[char], open: usize) -> Result<(String, usize)> {
    let mut depth = 1usize;
    let mut index = open + 1;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            '\'' => {
                index += 1;
                while index < chars.len() && chars[index] != '\'' {
                    index += 1;
                }
                index += 1;
            }
            '"' => {
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    if chars[index] == '\\' {
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                index += 1;
            }
            '(' => {
                depth += 1;
                index += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let script: String = chars[open + 1..index].iter().collect();
                    return Ok((script, index + 1));
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    bail!("unterminated command substitution: $(...)")
}

pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::ShellState;
    use crate::shell::exec::NullSink;

    fn state() -> ShellState {
        let mut state = ShellState::new(std::path::PathBuf::from("/home/test"), "test".to_owned());
        state.vars.insert("USER".to_owned(), "mobile".to_owned());
        state.vars.insert("EMPTY".to_owned(), String::new());
        state.last_status = 7;
        state
    }

    async fn expand(raw: &str, state: &ShellState) -> Option<String> {
        expand_word(raw, state, &mut NullSink).await.unwrap()
    }

    #[tokio::test]
    async fn quotes_and_escapes_collapse() {
        let state = state();
        assert_eq!(expand("'a b'", &state).await.unwrap(), "a b");
        assert_eq!(expand("\"c\\\"d\"", &state).await.unwrap(), "c\"d");
        assert_eq!(expand("a\\ b", &state).await.unwrap(), "a b");
        assert_eq!(expand("\"$USER\"", &state).await.unwrap(), "mobile");
        assert_eq!(expand("'$USER'", &state).await.unwrap(), "$USER");
    }

    #[tokio::test]
    async fn variables_and_status() {
        let state = state();
        assert_eq!(expand("$USER", &state).await.unwrap(), "mobile");
        assert_eq!(expand("${USER}!", &state).await.unwrap(), "mobile!");
        assert_eq!(expand("$?", &state).await.unwrap(), "7");
        assert_eq!(expand("x$UNSET", &state).await.unwrap(), "x");
        assert_eq!(expand("$UNSET", &state).await, None);
        assert_eq!(expand("\"$EMPTY\"", &state).await.unwrap(), "");
        assert_eq!(expand("''", &state).await.unwrap(), "");
    }

    #[tokio::test]
    async fn tilde_expands_at_the_start_only() {
        let state = state();
        assert_eq!(expand("~", &state).await.unwrap(), "/home/test");
        assert_eq!(expand("~/x", &state).await.unwrap(), "/home/test/x");
        assert_eq!(expand("a~b", &state).await.unwrap(), "a~b");
    }

    #[tokio::test]
    async fn unsupported_forms_fail_loudly() {
        let state = state();
        let error = expand_word("${USER:-x}", &state, &mut NullSink)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unsupported parameter"));
        let error = expand_word("$((1+2))", &state, &mut NullSink)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("arithmetic"));
    }

    #[tokio::test]
    async fn command_substitution_runs_in_a_subshell() {
        let state = state();
        let value = expand("$(echo hi; cd /; pwd)", &state).await.unwrap();
        assert_eq!(value, "hi\n/");
        // The outer state keeps its working directory.
        assert_eq!(state.cwd, std::path::PathBuf::from("/home/test"));
    }

    #[tokio::test]
    async fn command_substitution_strips_trailing_newlines() {
        let state = state();
        assert_eq!(expand("x$(echo hi)", &state).await.unwrap(), "xhi");
    }
}
