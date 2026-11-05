//! `session.resume_argv`: the command that resumes an agent's session, as the
//! runtime's restore table has it (`agent_resume.rs`). Argv only, never a
//! shell string; an id must have its agent's shape, so it can never be read
//! as an option, a path or a second word.

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
