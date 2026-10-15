//! Builds the command list from lexer tokens: pipelines joined by `;`, `&&`
//! and `||`, each command carrying its words and redirections.
//!
//! Grammar (everything else is a syntax error):
//!
//! ```text
//! list      := item*  (trailing `;` allowed)
//! item      := pipeline ( `;` | `&&` | `||` )?
//! pipeline  := command ( `|` command )*
//! command   := word+ redirect*     (words may be empty when redirects exist)
//! redirect  := `>` file | `>>` file | `<` file | `2>` file | `2>>` file | `2>&1`
//! ```

use super::lexer::Token;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    /// The pipeline ends the list or a `;`.
    End,
    Semi,
    /// Run the next pipeline only when this one succeeded.
    And,
    /// Run the next pipeline only when this one failed.
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    Stdin,
    Stdout,
    StdoutAppend,
    Stderr,
    StderrAppend,
    /// `2>&1`: send stderr wherever stdout currently goes.
    StderrToStdout,
}

#[derive(Debug, Clone)]
pub struct Redirect {
    pub kind: RedirectKind,
    /// Raw word, expanded when the command runs.
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub words: Vec<String>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub pipeline: Pipeline,
    pub sep: Sep,
}

#[derive(Debug, Clone)]
pub struct List {
    pub items: Vec<Item>,
}

pub fn parse(input: &str) -> Result<List, String> {
    let tokens = super::lexer::tokenize(input)?;
    let mut items = Vec::new();
    let mut commands: Vec<Command> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut redirects: Vec<Redirect> = Vec::new();
    // True right after a `|`: a separator or the end may not follow.
    let mut dangling_pipe = false;

    let flush =
        |words: &mut Vec<String>, redirects: &mut Vec<Redirect>, commands: &mut Vec<Command>| {
            if !words.is_empty() || !redirects.is_empty() {
                commands.push(Command {
                    words: std::mem::take(words),
                    redirects: std::mem::take(redirects),
                });
            }
        };
    let push_item = |commands: &mut Vec<Command>, items: &mut Vec<Item>, sep: Sep| {
        items.push(Item {
            pipeline: Pipeline {
                commands: std::mem::take(commands),
            },
            sep,
        });
    };

    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index] {
            Token::Word(word) => {
                words.push(word.clone());
                dangling_pipe = false;
                index += 1;
            }
            Token::RedirectOut
            | Token::RedirectAppend
            | Token::RedirectIn
            | Token::RedirectErr
            | Token::RedirectErrAppend => {
                let kind = match tokens[index] {
                    Token::RedirectOut => RedirectKind::Stdout,
                    Token::RedirectAppend => RedirectKind::StdoutAppend,
                    Token::RedirectIn => RedirectKind::Stdin,
                    Token::RedirectErr => RedirectKind::Stderr,
                    Token::RedirectErrAppend => RedirectKind::StderrAppend,
                    _ => unreachable!("the match arm above selected the redirect"),
                };
                let Some(Token::Word(target)) = tokens.get(index + 1) else {
                    return Err(format!(
                        "expected a file name after '{}'",
                        kind_symbol(kind)
                    ));
                };
                redirects.push(Redirect {
                    kind,
                    target: target.clone(),
                });
                dangling_pipe = false;
                index += 2;
            }
            Token::RedirectErrToOut => {
                redirects.push(Redirect {
                    kind: RedirectKind::StderrToStdout,
                    target: String::new(),
                });
                index += 1;
            }
            Token::Pipe => {
                flush(&mut words, &mut redirects, &mut commands);
                if dangling_pipe {
                    return Err("expected a command between '|' operators".to_owned());
                }
                dangling_pipe = true;
                index += 1;
            }
            Token::Semi | Token::And | Token::Or => {
                let sep = match tokens[index] {
                    Token::Semi => Sep::Semi,
                    Token::And => Sep::And,
                    Token::Or => Sep::Or,
                    _ => unreachable!("the match arm above selected the separator"),
                };
                flush(&mut words, &mut redirects, &mut commands);
                if dangling_pipe {
                    return Err("expected a command after '|'".to_owned());
                }
                if commands.is_empty() {
                    return Err(format!("unexpected '{}'", sep_symbol(sep)));
                }
                push_item(&mut commands, &mut items, sep);
                index += 1;
            }
        }
    }
    flush(&mut words, &mut redirects, &mut commands);
    if dangling_pipe {
        return Err("expected a command after '|'".to_owned());
    }
    if !commands.is_empty() {
        push_item(&mut commands, &mut items, Sep::End);
    } else if let Some(last) = items.last_mut() {
        match last.sep {
            Sep::And => return Err("expected a command after '&&'".to_owned()),
            Sep::Or => return Err("expected a command after '||'".to_owned()),
            Sep::Semi => last.sep = Sep::End,
            Sep::End => {}
        }
    }
    Ok(List { items })
}

fn kind_symbol(kind: RedirectKind) -> &'static str {
    match kind {
        RedirectKind::Stdin => "<",
        RedirectKind::Stdout => ">",
        RedirectKind::StdoutAppend => ">>",
        RedirectKind::Stderr => "2>",
        RedirectKind::StderrAppend => "2>>",
        RedirectKind::StderrToStdout => "2>&1",
    }
}

fn sep_symbol(sep: Sep) -> &'static str {
    match sep {
        Sep::End => "end",
        Sep::Semi => ";",
        Sep::And => "&&",
        Sep::Or => "||",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &Command) -> Vec<&str> {
        command.words.iter().map(String::as_str).collect()
    }

    #[test]
    fn parses_a_three_stage_pipeline() {
        let list = parse("cat a | tr a-z A-Z | head -1").unwrap();
        assert_eq!(list.items.len(), 1);
        let pipeline = &list.items[0].pipeline;
        assert_eq!(pipeline.commands.len(), 3);
        assert_eq!(words(&pipeline.commands[0]), ["cat", "a"]);
        assert_eq!(words(&pipeline.commands[2]), ["head", "-1"]);
        assert_eq!(list.items[0].sep, Sep::End);
    }

    #[test]
    fn chains_separators_and_keeps_trailing_semi() {
        let list = parse("a && b || c ; d;").unwrap();
        let seps: Vec<Sep> = list.items.iter().map(|item| item.sep).collect();
        assert_eq!(seps, [Sep::And, Sep::Or, Sep::Semi, Sep::End]);
    }

    #[test]
    fn attaches_redirections_to_their_command() {
        let list = parse("echo hi > out.txt 2>&1").unwrap();
        let command = &list.items[0].pipeline.commands[0];
        assert_eq!(command.redirects.len(), 2);
        assert_eq!(command.redirects[0].kind, RedirectKind::Stdout);
        assert_eq!(command.redirects[0].target, "out.txt");
        assert_eq!(command.redirects[1].kind, RedirectKind::StderrToStdout);
    }

    #[test]
    fn bare_redirect_is_a_command() {
        let list = parse("> created").unwrap();
        let command = &list.items[0].pipeline.commands[0];
        assert!(command.words.is_empty());
        assert_eq!(command.redirects[0].kind, RedirectKind::Stdout);
    }

    #[test]
    fn rejects_incomplete_constructs() {
        assert!(parse("a |").is_err());
        assert!(parse("a | ; b").is_err());
        assert!(parse("; a").is_err());
        assert!(parse("a &&").is_err());
        assert!(parse("a > ").is_err());
        assert!(parse("a >").is_err());
    }

    #[test]
    fn empty_input_is_an_empty_list() {
        assert!(parse("   ").unwrap().items.is_empty());
    }
}
