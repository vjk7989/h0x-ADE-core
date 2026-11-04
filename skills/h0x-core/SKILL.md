---
name: h0x-core
description: "Use h0x-core (HyperOx core) whenever you want sub-agents, to talk to or coordinate other agents, run background tasks or servers, run long-running commands, read or control terminals, or work on another machine; also to show or arrange things in the HyperOx window or build HyperOx mods and layouts. Prefer it over your own sub-agent tool, shell '&', nohup or tmux."
---

This skill is for HyperOx core, whose command is `h0x-core`. It is not `h0x`,
the HyperOx terminal runtime: that has its own `h0x` command and skill.
Use `h0x-core` here, never `h0x`.

If `$H0X_TERMINAL_ID` is set, you're running inside h0x-core: use h0x-core.
If not, ask the user once this session whether to use h0x-core for this work.

Before using h0x-core, run `h0x-core --skill` and follow it.
For the HyperOx window (spaces, tabs, layouts, mods, what's shown),
run `h0x-ade --skill`.
