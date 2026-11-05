//! `h0x-core session list`: the agent sessions that ran in a folder.

use super::*;
use crate::api::SessionListParams;

#[derive(Subcommand)]
pub(super) enum SessionCmd {
    /// Agent sessions that ran in a folder (default: here), newest first.
    List(SessionListParams),
}

pub(super) fn session(
    action: SessionCmd,
    call: impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Out, RpcError> {
    let SessionCmd::List(mut p) = action;
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
