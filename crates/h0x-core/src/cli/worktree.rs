//! `h0x-core worktree list|create|remove|merge`: git worktrees of a repository, done by the
//! `h0x-wt` shipped beside this binary. Hidden: the app's Worktree dialog uses the API methods.

use super::*;
use crate::api::{WorktreeCreateParams, WorktreeListParams, WorktreeMergeParams, WorktreeRemoveParams};

#[derive(Subcommand)]
pub(super) enum WorktreeCmd {
    /// The worktrees of a repository with their status, main checkout first.
    List(WorktreeListParams),
    /// Make a worktree for a branch (new from --base, or the existing one).
    Create(WorktreeCreateParams),
    /// Remove a worktree; its branch goes too only when its work is in the default branch.
    Remove(WorktreeRemoveParams),
    /// Merge a worktree's branch into the default branch (squashed), then remove it.
    Merge(WorktreeMergeParams),
}

pub(super) fn worktree(
    action: WorktreeCmd,
    call: impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Out, RpcError> {
    // Relative folders mean the caller's folder, which the core (maybe elsewhere) cannot know.
    let here = |on: &Option<String>, path: String| {
        if on.is_none() { from_here(Some(path)).unwrap_or_default() } else { path }
    };
    Ok(match action {
        WorktreeCmd::List(mut p) => {
            p.folder = here(&p.on, p.folder);
            let r = call("worktree.list", to(&p))?;
            let rows: Vec<String> = r["worktrees"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|w| {
                    let branch = w["branch"].as_str().unwrap_or("(detached)");
                    let flag = if w["dirty"] == true { " *" } else { "" };
                    format!("{branch:<24} {}{flag}", w["path"].as_str().unwrap_or_default())
                })
                .collect();
            Out { json: r, human: rows.join("\n") }
        }
        WorktreeCmd::Create(mut p) => {
            p.folder = here(&p.on, p.folder);
            let r = call("worktree.create", to(&p))?;
            let human = r["path"].as_str().unwrap_or_default().to_string();
            Out { json: r, human }
        }
        WorktreeCmd::Remove(mut p) => {
            p.path = here(&p.on, p.path);
            let r = call("worktree.remove", to(&p))?;
            let human = format!("removed {}", r["path"].as_str().unwrap_or_default());
            Out { json: r, human }
        }
        WorktreeCmd::Merge(mut p) => {
            p.path = here(&p.on, p.path);
            let r = call("worktree.merge", to(&p))?;
            let human = format!("merged into {}", r["target"].as_str().unwrap_or_default());
            Out { json: r, human }
        }
    })
}
