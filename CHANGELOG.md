# Changelog

## [Unreleased]

### New

- Remote control (bridge) groundwork: `bridge pair|unpair|ls|lock|unlock|log` pair chat accounts with one-time codes. Paired accounts can list, read, send, press keys, approve or deny, interrupt, run and kill. Destructive commands need a single-use confirmation, and every command is rate limited, redacted and audited. See docs/bridge.md.
- Telegram bot for the bridge: `bridge telegram token` stores the bot token (0600, never shown), and `bridge run` long-polls Telegram through the system curl, outbound and https only. It sends notices with Approve, Deny, Show and Interrupt buttons, answers `/agents`, `/read`, `/send`, `/keys`, `/approve`, `/deny`, `/interrupt`, `/run`, `/kill` and `/lock` from paired accounts in private chats, and asks for a confirmation tap before destructive commands.

## [0.1.5] - 2026-11-05

### New

- Agent sessions: `session list`, `session resume` and `session trash` (API `session.list`, `session.resume_argv`, `session.trash`) find, resume and remove past Claude Code, Codex and OpenCode sessions.
- Trashing a session moves its files to the Recycle Bin or Trash; it never deletes them permanently.

### Fixed

- Stray mouse and focus reports no longer reach the shell after a full-screen program exits.

## [0.1.4] - 2026-11-05

### Changed

- Includes the herdr-core 0.1.18 changes: a terminal's environment survives planned core restarts, terminals start with the display's current facts, and wait/template fixes.
- `terminal.run` accepts the caller's colors and cell size, so a program's display queries are answered from its first moment.
- The core raises its open file limit at startup, so many terminals at once no longer run out of file handles.
- The macOS signing and notarization changes from herdr-core 0.1.18 were not taken: HyperOx ships unsigned GitHub Releases.

### Fixed

- A template's first message is retried if the program is not ready yet; if it still cannot be sent, the terminal keeps it so it can be inspected.
- Waiting on a terminal's children waits for the tasks sent to them to finish, keeps finished tasks across a planned core restart, and ignores replayed startup reports.

## [0.1.3] - 2026-11-05

### Changed

- Faster terminal output: fewer copies and system calls between terminals and the apps watching them.
- The notices readme names h0x-core.

## [0.1.2] - 2026-11-05

### Fixed

- Fixed (security): a remote machine you ran a command on could gain the right to type into any of your local terminals; now only the terminal that called it.
- Remote machines can no longer force input into a terminal waiting on you, and their input is always labelled with the machine.
- Remote machines can no longer show their terminals as yours.
- Pasted text can no longer end bracketed paste early.
- Release files must be signed for that exact file, and a release list for another version is refused.

## [0.1.1] - 2026-11-05

### New

- Updates are signed: h0x-core checks a minisign signature on every release it downloads.
- Downloads go only over HTTPS and only from GitHub's release hosts.

### Fixed

- ssh and scp targets can no longer be read as options.
- Socket folders are private to your user and checked for their owner, sockets are readable by you alone (0600), and on Windows h0x-core refuses a pipe served by another account.
- The release-URL and ssh overrides work only in debug and test builds.
