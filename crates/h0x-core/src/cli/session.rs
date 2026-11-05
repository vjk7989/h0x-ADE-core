//! `h0x-core session list|resume|trash`: agent sessions of a folder, the
//! command that resumes one, and moving one to the trash.

use super::*;
use crate::api::{SessionListParams, SessionParams};

#[derive(Subcommand)]
pub(super) enum SessionCmd {
    /// Agent sessions that ran in a folder (default: here), newest first.
    List(SessionListParams),
    /// The command that resumes a session (run it in the folder it ran in).
    Resume(SessionParams),
    /// Move a session's files to the trash (Recycle Bin, Trash), never deleting them.
    Trash(SessionParams),
}

pub(super) fn session(
    action: SessionCmd,
    call: impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Out, RpcError> {
    let mut p = match action {
        SessionCmd::List(p) => p,
        SessionCmd::Resume(p) => {
            let r = call("session.resume_argv", to(&p))?;
            let words: Vec<&str> = r["argv"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let human = words.join(" ");
            return Ok(Out { json: r, human });
        }
        SessionCmd::Trash(p) => {
            let r = call("session.trash", to(&p))?;
            let paths: Vec<&str> = r["trashed"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let human = format!("moved to the trash:\n{}", paths.join("\n"));
            return Ok(Out { json: r, human });
        }
    };
    if p.on.is_none() {
        p.folder = from_here(Some(p.folder)).unwrap_or_default();
    }
    let r = call("session.list", to(&p))?;
    let rows: Vec<String> = r["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let field = |key: &str| s[key].as_str().unwrap_or_default().to_string();
            format!("{:<9} {:<36} {}", field("agent"), field("id"), field("title"))
        })
        .collect();
    let human = if rows.is_empty() { "no agent sessions ran here".into() } else { rows.join("\n") };
    Ok(Out { json: r, human })
}
