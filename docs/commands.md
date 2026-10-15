# The `bash` command engine

iOS gives sandboxed apps no way to `exec` anything - not even a binary shipped
inside the app bundle (`posix_spawn` fails with `EPERM`). The Worker therefore
runs commands **inside the app process**: a small POSIX-style interpreter and
a fixed set of native commands. This page documents what the engine accepts;
anything outside it fails with an explicit error instead of pretending.

The engine is deliberately a subset of `sh`, not a full shell. It keeps the
`bash` tool contract: `{command, timeout_seconds}` in,
`{exit_code, stdout, stderr}` out, output capped at 2 MiB per call.

## Grammar

- **Pipelines**: `cmd1 | cmd2 | cmd3`. Stages run concurrently and exchange
  bounded chunks; a consumer that exits early (like `head`) stops the
  producer, and an unbounded producer (`yes`) stays bounded.
- **Lists**: `;` runs the next command unconditionally, `&&` only after
  success, `||` only after failure. `exit N` stops the list with status `N`.
- **Redirections**: `> file` (truncate), `>> file` (append), `< file`,
  `2> file`, `2>> file`, and `2>&1` (send stderr wherever stdout goes).
  `/dev/null`, `/dev/stdout` and `/dev/stderr` are recognized as targets.
  A bare redirection (`> file`) creates or truncates the file.
- **Variable prefixes**: `NAME=value cmd` applies to that one command;
  `NAME=value` alone persists in the shell (this call only).

## Expansion

- Quotes: `'single'` (literal) and `"double"` (expansions inside); quotes
  are removed before the command runs.
- Escapes: `\c` outside quotes, `\"`, `\\`, `\$` inside double quotes.
- Variables: `$NAME`, `${NAME}`, `$?` (last exit status), `$$` (process id).
- Command substitution: `$(...)` runs a nested list in a subshell: changes to
  variables or the directory do not leak out, trailing newlines are removed
  from the output, and nested stderr passes through.
- `~` at the start of a word expands to `HOME`.
- Deliberately **not** supported (they stay literal or fail): arithmetic
  `$((...))`, brace forms like `${VAR:-default}`, `~user`, backquotes, field
  splitting (an unquoted `$VAR` is never split on whitespace), and pathname
  expansion - the shell does not expand `*` itself, but commands like `find`
  and `grep` match patterns themselves.
- There are no loops, `if` statements, functions, subshells `( )`, background
  jobs (`&`), heredocs, or here-strings.

## Command reference

Runs in the app container; `HOME` and the working directory are
`Documents/CybionWorker` (visible in the Files app). Paths outside the app
sandbox fail the way iOS denies them.

| Command | Notes |
| --- | --- |
| `:` | Do nothing, successfully. |
| `[` / `test` | Conditionals: `-e -f -d -s -r -w -x -L -z -n`, `=` `!=` `<` `>` (escape the angle brackets), `-eq -ne -lt -le -gt -ge`, leading `!`. |
| `basename` | Last path component; optional suffix to strip. |
| `cat` | Concatenate files or stdin (`-`); keeps going after a bad file. |
| `cd` | Change directory; no argument goes to `HOME`; sets `PWD`. |
| `cp` | `-r`/`-R` to copy directories; refuses `omitting directory` without it. |
| `cut` | `-d DELIM`, `-f LIST`, `-c LIST`, `-s`; `LIST` like `1,3-5,7-`. |
| `date` | `-u`, `+FORMAT` (strftime, plus `%s`); default format like `date(1)`. |
| `dirname` | Path without the last component. |
| `du` | Disk usage in 512-byte blocks; `-s` summary, `-h` human sizes. |
| `echo` | `-n`, `-e`/`-E` for escape processing. |
| `env` | No argument lists variables; with names prints their values. |
| `eval` | Run the arguments as a command list in this shell. |
| `exit` | `exit [N]` stops the current list with status `N`. |
| `export` | `export NAME=value`, `export NAME`, or list; values are not split. |
| `find` | `-name`/`-iname` (shell-style pattern), `-type f|d|l`, `-maxdepth`, `-mindepth`, `-print`; skips symlinked directories. |
| `grep` | `-i -n -v -c -l -r -F -q`, `-e PATTERN`, `--`; regular expressions by default (Rust regex syntax). |
| `head` | `-n N` or `-c N` (default 10 lines); headers with multiple files. |
| `hostname` | The device name as configured. |
| `ls` | `-l -a -h -1`; one entry per line (`-l` adds mode, links, uid/gid, size, time, and `->` for symlinks). No `-R`. |
| `mkdir` | `-p` creates parents. |
| `mv` | Moves into a directory target; falls back to copy+delete across volumes. |
| `open` | Opens a URL through the system (same callback as `computer_use` `open`). |
| `printf` | `%s %d %i %u %x %X %o %c %f` with width, precision and `-`/`0` flags; `\n` `\t` `\0NNN` `\xHH` escapes; the format repeats while arguments remain. |
| `pwd` | Print the working directory. |
| `readlink` | `-f` canonicalizes; otherwise prints the link target. |
| `rm` | `-r`/`-R` recursive, `-f` ignores missing files; refuses directories without `-r`. |
| `rmdir` | Remove empty directories. |
| `sed` | `s/re/repl/[gi]` (with `&` and `\1`..`\9`), `p`, `d`, `q`, addresses `N`, `$`, `/re/` and ranges `a,b`; `-n`, `-e SCRIPT`, `-i` (with optional suffix). |
| `seq` | `seq LAST`, `seq FIRST LAST`, `seq FIRST STEP LAST`; integers print without decimals. |
| `set` | No arguments lists the variables; options are not supported. |
| `sh` | `sh -c 'cmd'` runs a nested list (same engine); without `-c` reads the script from stdin. |
| `sleep` | Seconds, optionally with `s`, `m`, `h`, `d` suffixes; fractional values work. |
| `sort` | `-n` numeric, `-r` reverse, `-u` unique. |
| `stat` | Default view (size, type, mode, mtime) or `-c` with `%n %s %a %F %Y %y`. |
| `tail` | `-n N` or `-c N`; `-f` is rejected. |
| `touch` | `-c` (no create); sets mtimes to now. |
| `tr` | `-d` deletes; otherwise translates; `a-z` ranges expand. |
| `true` / `false` | Exit 0 / 1. |
| `uname` | `-a -s -n -r -v -m`; default prints `Darwin`. |
| `uniq` | `-c -d -u` on adjacent lines. |
| `unset` | Remove variables. |
| `wc` | `-l -w -c -m`; defaults to lines, words, bytes; totals with multiple files. |
| `which` | Prints names known to the engine (it resolves built-ins, not files). |
| `whoami` | Always `mobile`: the sandbox user. |
| `yes` | Repeats a line until the consumer exits or the call is cancelled. |

## Exit status and errors

- The result's `exit_code` is the last command's status; `127` means an
  unknown command, `141` a stage stopped by a broken pipe.
- Operational errors print `command: message` on stderr and exit `1`;
  `grep` returns `2` for file errors and `1` when nothing matched.
- Syntax and expansion errors exit `2` and start with `sh:`.
- Cancellation or the timeout end the call with an explicit tool error
  (`Bash command cancelled by the Controller`, `Bash command timed out`).
