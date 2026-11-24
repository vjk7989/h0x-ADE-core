//! `session.resume_argv`: the command that resumes an agent's session, as the
//! runtime's restore table has it (`agent_resume.rs`). Argv only, never a
//! shell string; an id must have its agent's shape, so it can never be read
//! as an option, a path or a second word.

use std::path::Path;

/// Longer ids are refused.
pub(super) const MAX_ID: usize = 512;
const MAX_ARGS: usize = 64;
const MAX_ARGV_BYTES: usize = 8192;

fn flag(flag: &str, id: &str) -> Vec<String> {
    vec![flag.to_string(), id.to_string()]
}

/// How an agent's session ids look.
#[derive(Clone, Copy)]
enum Shape {
    /// `8-4-4-4-12` hex digits.
    Uuid,
    /// This prefix, then ASCII letters and digits.
    Prefixed(&'static str),
    /// An ASCII letter or digit, then letters, digits, `-`, `_` or `.`.
    Token,
}

type Row = (&'static str, Shape, fn(&str) -> Vec<String>);

/// The agent's resume command, its id's shape, and the argv after the command.
fn table(agent: &str) -> Option<Row> {
    let row: Row = match agent {
        "claude" => ("claude", Shape::Uuid, |id| flag("--resume", id)),
        "codex" => ("codex", Shape::Uuid, |id| flag("resume", id)),
        "copilot" => ("copilot", Shape::Uuid, |id| vec![format!("--resume={id}")]),
        "devin" => ("devin", Shape::Token, |id| flag("--resume", id)),
        "droid" => ("droid", Shape::Token, |id| flag("--resume", id)),
        "kimi" => ("kimi", Shape::Token, |id| flag("--session", id)),
        "mastracode" => ("mastracode", Shape::Token, |id| flag("--thread", id)),
        "pi" => ("pi", Shape::Token, |id| flag("--session", id)),
        // omp has no `--session`, unlike pi.
        "omp" => ("omp", Shape::Token, |id| vec![format!("--resume={id}")]),
        "hermes" => ("hermes", Shape::Token, |id| flag("--resume", id)),
        "opencode" => ("opencode", Shape::Prefixed("ses_"), |id| flag("--session", id)),
        "qodercli" => ("qodercli", Shape::Token, |id| flag("--resume", id)),
        "qwen" => ("qwen", Shape::Uuid, |id| flag("--resume", id)),
        "kilo" => ("kilo", Shape::Token, |id| flag("--session", id)),
        "cursor" => {
            (if cfg!(windows) { "cursor-agent.cmd" } else { "cursor-agent" }, Shape::Token, |id| flag("--resume", id))
        }
        "agy" => ("agy", Shape::Token, |id| flag("--conversation", id)),
        "grok" => ("grok", Shape::Token, |id| flag("--resume", id)),
        // `default:<agent id>` is the agent's default conversation.
        "letta" => ("letta", Shape::Token, |id| match id.strip_prefix("default:") {
            Some(agent) => vec!["--conversation".into(), "default".into(), "--agent".into(), agent.into()],
            None => flag("--conversation", id),
        }),
        _ => return None,
    };
    Some(row)
}

/// The argv that resumes `agent`'s session `id`.
pub fn argv(agent: &str, id: &str) -> Result<Vec<String>, String> {
    let Some((command, shape, args)) = table(agent) else {
        return Err(format!("no resume command for agent {agent:?}"));
    };
    // Letta's own default conversation names the agent after `default:`.
    let checked = if agent == "letta" { id.strip_prefix("default:").unwrap_or(id) } else { id };
    // The limit holds for the whole id, `default:` included.
    if id.len() > MAX_ID || !shaped(checked, shape) {
        return Err(format!("{id:?} is not a {agent} session id"));
    }
    let mut argv = vec![command.to_string()];
    argv.extend(args(id));
    validate(&argv)?;
    Ok(argv)
}

/// Whether `id` is an id of this shape, at most [`MAX_ID`] bytes.
fn shaped(id: &str, shape: Shape) -> bool {
    let word = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    id.len() <= MAX_ID
        && match shape {
            Shape::Uuid => {
                id.len() == 36
                    && id.bytes().enumerate().all(|(i, b)| match i {
                        8 | 13 | 18 | 23 => b == b'-',
                        _ => b.is_ascii_hexdigit(),
                    })
            }
            Shape::Prefixed(prefix) => id.strip_prefix(prefix).is_some_and(word),
            Shape::Token => {
                id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                    && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            }
        }
}

/// The runtime's check before it types a resume command into a shell: a bare
/// command name, bounded, no control characters or apostrophes.
fn validate(argv: &[String]) -> Result<(), String> {
    let Some(command) = argv.first() else { return Err("resume argv must not be empty".into()) };
    if argv.len() > MAX_ARGS {
        return Err(format!("resume argv allows at most {MAX_ARGS} arguments"));
    }
    if argv.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES {
        return Err(format!("resume argv allows at most {MAX_ARGV_BYTES} bytes"));
    }
    if argv.iter().any(|arg| arg.chars().any(char::is_control) || arg.contains('\'')) {
        return Err("resume argv must not contain control characters or apostrophes".into());
    }
    let plain = !command.is_empty()
        && !command.starts_with('-')
        && command.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if !plain {
        return Err("resume argv must start with a plain command name, not a path".into());
    }
    Ok(())
}

/// The flags an agent's restore keeps, as written in its own `--help`.
#[derive(Clone, Copy)]
pub(crate) struct FlagLists {
    /// Flags that stand alone.
    pub(crate) bool_flags: &'static [&'static str],
    /// Flags followed by one value (`--flag value` or `--flag=value`).
    pub(crate) value_flags: &'static [&'static str],
}

const MAX_KEPT: usize = 32;
const MAX_VALUE: usize = 4096;
/// Never kept, whatever an allowlist says: they name another session or run a prompt.
const NEVER: [&str; 14] = [
    "--resume",
    "--continue",
    "-c",
    "--session",
    "--session-id",
    "--fork-session",
    "--conversation",
    "--thread",
    "-p",
    "--print",
    "--prompt",
    "-i",
    "--output-format",
    "--settings", // the shim adds HyperOx's again; `--settings=x` is split to this name too
];

/// The allowlisted flags of an agent's argv (after its program token), in
/// order: no prompt, file, subcommand, session flag or anything after `--`.
#[allow(dead_code)] // read by the resume path in later sub-tasks
pub(super) fn keep_flags(args: &[String], lists: &FlagLists) -> Vec<String> {
    let sound = |v: &str| v.len() <= MAX_VALUE && !v.chars().any(char::is_control);
    let mut kept: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() && args[i] != "--" {
        let before = kept.len();
        let arg = &args[i];
        i += 1;
        if !arg.starts_with('-') {
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg.as_str(), None),
        };
        // The value of a flag that is not kept goes with it.
        let next = args.get(i).filter(|n| !n.starts_with('-') && n.as_str() != "--");
        let skip = usize::from(inline.is_none() && next.is_some());
        if NEVER.contains(&name) {
            i += skip;
        } else if lists.bool_flags.contains(&name) && inline.is_none() && sound(arg) {
            kept.push(arg.clone());
        } else if lists.value_flags.contains(&name) {
            match (inline, next) {
                (Some(value), _) if !value.is_empty() && sound(value) && sound(name) => kept.push(arg.clone()),
                (None, Some(value)) if sound(value) => {
                    i += 1;
                    kept.extend([arg.clone(), value.clone()]);
                }
                _ => i += skip,
            }
        } else {
            i += skip;
        }
        if kept.len() > MAX_KEPT {
            kept.truncate(before);
            break;
        }
    }
    kept
}

/// One line that runs `argv` when typed into the shell at `shell`: each
/// element is one word. `None` for cmd.exe, an unknown shell, an empty argv or
/// an element with a control character (it would end the line early).
#[allow(dead_code)] // read by the resume path in later sub-tasks
pub(super) fn shell_line(argv: &[String], shell: &Path) -> Option<String> {
    if argv.is_empty() || argv.iter().any(|a| a.chars().any(char::is_control)) {
        return None;
    }
    // By hand: a Windows path is one path on every host.
    let shell = shell.to_string_lossy();
    let name = shell.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase();
    let stem = name.rsplit_once('.').map_or(name.as_str(), |(stem, _)| stem);
    match stem {
        "sh" | "bash" | "zsh" | "dash" => {
            let words: Vec<String> = argv.iter().map(|a| format!("'{}'", a.replace('\'', r"'\''"))).collect();
            Some(words.join(" "))
        }
        // Fish reads `\\` and `\'` as escapes inside single quotes.
        "fish" => {
            let words: Vec<String> =
                argv.iter().map(|a| format!("'{}'", a.replace('\\', r"\\").replace('\'', r"\'"))).collect();
            Some(words.join(" "))
        }
        // Legacy native-argument passing (ADR-060) for this command only: pwsh 7.3+ defaults to Standard.
        "pwsh" | "powershell" => {
            let words: Vec<String> = argv.iter().map(|a| powershell_word(a)).collect();
            Some(format!("& {{ $PSNativeCommandArgumentPassing = 'Legacy'; & {} }}", words.join(" ")))
        }
        _ => None,
    }
}

/// A PowerShell single-quoted word; quotes doubled, curly ones too.
fn powershell_word(arg: &str) -> String {
    let arg = if arg.contains('"') { windows_arg(arg) } else { arg.to_string() };
    let mut word = String::from("'");
    for c in arg.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            word.push(c);
        }
        word.push(c);
    }
    word + "'"
}

/// One argument of a Windows command line, as C runtimes read it (ADR-060):
/// a `"` inside quotes is written `""`; backslashes before one are doubled.
fn windows_arg(arg: &str) -> String {
    let mut s = String::from('"');
    let mut slashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                slashes += 1;
                continue;
            }
            '"' => s.extend(std::iter::repeat_n('\\', 2 * slashes).chain(['"', '"'])),
            _ => s.extend(std::iter::repeat_n('\\', slashes).chain([ch])),
        }
        slashes = 0;
    }
    s.extend(std::iter::repeat_n('\\', 2 * slashes));
    s + "\""
}
