//! Tokenizer for the supported grammar.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A word, kept raw (quotes included) for the expansion stage.
    Word(String),
    Semi,
    And,
    Or,
    Pipe,
    RedirectOut,
    RedirectAppend,
    RedirectIn,
    RedirectErr,
    RedirectErrAppend,
    /// `2>&1`.
    RedirectErrToOut,
}

/// Consumes `$(...)` contents into `word` (the `$(` is already pushed),
/// tracking nesting and quotes until the matching `)`.
// `while let` on purpose: escape handling needs `chars.next()` inside the loop.
#[allow(clippy::while_let_on_iterator)]
fn scan_substitution(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    word: &mut String,
) -> Result<(), String> {
    let mut depth = 1usize;
    while let Some(character) = chars.next() {
        word.push(character);
        match character {
            '\\' => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            '\'' => {
                for next in chars.by_ref() {
                    word.push(next);
                    if next == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                while let Some(next) = chars.next() {
                    word.push(next);
                    if next == '\\' {
                        if let Some(escaped) = chars.next() {
                            word.push(escaped);
                        }
                    } else if next == '"' {
                        break;
                    }
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    Err("unterminated command substitution".to_owned())
}

pub fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut chars = input.chars().peekable();
    let mut unquoted = true;
    let mut quote: Option<char> = None;
    let flush = |word: &mut String, tokens: &mut Vec<Token>| {
        if !word.is_empty() {
            tokens.push(Token::Word(std::mem::take(word)));
        }
    };
    while let Some(character) = chars.next() {
        match quote {
            Some('\'') => {
                word.push(character);
                if character == '\'' {
                    quote = None;
                }
            }
            Some('"') => {
                word.push(character);
                if character == '"' {
                    quote = None;
                } else if character == '\\'
                    && let Some(next) = chars.next()
                {
                    word.push(next);
                }
            }
            _ => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    word.push(character);
                    unquoted = false;
                }
                '\\' => {
                    word.push(character);
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                }
                '$' if chars.peek() == Some(&'(') => {
                    // Command substitution: spaces and operators inside it
                    // belong to this word.
                    chars.next();
                    word.push_str("$(");
                    scan_substitution(&mut chars, &mut word)?;
                }
                ' ' | '\t' | '\n' => {
                    flush(&mut word, &mut tokens);
                    unquoted = true;
                }
                ';' => {
                    flush(&mut word, &mut tokens);
                    tokens.push(Token::Semi);
                }
                '|' => {
                    flush(&mut word, &mut tokens);
                    if chars.peek() == Some(&'|') {
                        chars.next();
                        tokens.push(Token::Or);
                    } else {
                        tokens.push(Token::Pipe);
                    }
                }
                '&' => {
                    flush(&mut word, &mut tokens);
                    if chars.peek() == Some(&'&') {
                        chars.next();
                        tokens.push(Token::And);
                    } else {
                        return Err("background jobs (&) are not supported".to_owned());
                    }
                }
                '>' => {
                    flush(&mut word, &mut tokens);
                    if chars.peek() == Some(&'>') {
                        chars.next();
                        tokens.push(Token::RedirectAppend);
                    } else {
                        tokens.push(Token::RedirectOut);
                    }
                }
                '<' => {
                    flush(&mut word, &mut tokens);
                    tokens.push(Token::RedirectIn);
                }
                '2' if word.is_empty() && chars.peek() == Some(&'>') => {
                    // `2>` / `2>>` / `2>&1`: only when starting a fresh word.
                    chars.next();
                    if chars.peek() == Some(&'>') {
                        chars.next();
                        tokens.push(Token::RedirectErrAppend);
                    } else if chars.peek() == Some(&'&') {
                        chars.next();
                        if chars.peek() == Some(&'1') {
                            chars.next();
                            tokens.push(Token::RedirectErrToOut);
                        } else {
                            return Err("only '2>&1' is supported after '2>'".to_owned());
                        }
                    } else {
                        tokens.push(Token::RedirectErr);
                    }
                }
                _ => {
                    word.push(character);
                    let _ = unquoted;
                }
            },
        }
    }
    if quote.is_some() {
        return Err("unterminated quote".to_owned());
    }
    flush(&mut word, &mut tokens);
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_operators_and_keeps_quotes() {
        let tokens = tokenize("echo 'a b' && cat x | head > out").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Word("echo".into()),
                Token::Word("'a b'".into()),
                Token::And,
                Token::Word("cat".into()),
                Token::Word("x".into()),
                Token::Pipe,
                Token::Word("head".into()),
                Token::RedirectOut,
                Token::Word("out".into()),
            ]
        );
    }

    #[test]
    fn rejects_background_jobs() {
        assert!(tokenize("sleep 1 &").is_err());
    }

    #[test]
    fn keeps_command_substitutions_and_merges_in_one_word() {
        let tokens = tokenize("echo $(echo a b) | cat").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Word("echo".into()),
                Token::Word("$(echo a b)".into()),
                Token::Pipe,
                Token::Word("cat".into()),
            ]
        );
        let tokens = tokenize("cmd 2>&1").unwrap();
        assert_eq!(
            tokens,
            vec![Token::Word("cmd".into()), Token::RedirectErrToOut]
        );
    }
}
