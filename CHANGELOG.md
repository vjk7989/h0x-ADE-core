# Changelog

## [Unreleased]

## [0.1.10] - 2026-11-09

Security release from the second audit (Cloudflare security-audit tool).

### Changed

- Install scripts check a new download with the h0x-core you already have (0.1.9 or newer); only a first install trusts the downloaded core's own check. Calls to the installed core time out instead of hanging, and odd version numbers are handled safely.
- `update --from` unpacks exactly the bytes it verified.
- Notes from another machine arrive labelled with that machine's name; a forged note cannot pose as a local agent.
- Behind your own VPS proxy (`--via`), each visitor is counted by the proxy's `X-Forwarded-For` address; one visitor can hold at most 8 unauthenticated connections; floods of failed attempts are summarised in the audit log instead of pushing real entries out.
- A phone you unpair is disconnected within 20 seconds.

### Fixed

- Every audit-log write respects the size cap.
- Exit and error notes escape the program's output, and stray carriage returns can no longer act as Enter.
- Windows: an old pipe name taken by another account no longer stops the core from starting.

## [0.1.9] - 2026-11-09

Security release from a full audit.

### Changed

- Phone panel: every way of reaching it (Wi-Fi, Cloudflare, your VPS) now says who delivers the page; `bridge web --json` has a `warning` field.
- Phone sessions expire 30 days after pairing.
- Wrong pairing codes are limited per sender, web and Telegram pairing no longer block each other, the panel accepts at most 64 connections, and the audit log rotates at 1 MiB.
- `update --from` and the install scripts verify the release signature before installing; `install.ps1` locks an install folder outside your profile to you, SYSTEM and Administrators.
- Telegram/web approvals for a plain shell always ask for confirmation, even if a program claims to be an agent.
- Messages between agents can no longer forge a second message or carry terminal control codes.

### Fixed

- An install-folder `cloudflared` is checked against its known fingerprint before every start.
- A temporary secret-store problem no longer replaces the panel's key.
- Windows: the core's pipe name is unique per user, and a terminal that cannot join its clean-up job is not started.

## [0.1.8] - 2026-11-08

### New

- The phone panel is end-to-end encrypted. Everything between your phone and this computer goes through an encrypted channel (P-256 key agreement, HKDF-SHA256, AES-256-GCM), so the Wi-Fi, Cloudflare or your VPS only see ciphertext.
- The QR link carries this computer's fingerprint. Your phone remembers it and refuses a different computer, so nobody in between can pretend to be yours.
- Each paired phone is bound to its own device key; a stolen session token alone does not log in.
- Bridge secrets (the Telegram bot token and the panel's server key) are kept in the system's secret store: Windows DPAPI, the macOS Keychain or the Linux Secret Service, with an owner-only file as the fallback. Tokens saved by older versions are moved in automatically.

### Changed

- Phones paired with an older version must scan the QR code once more.
- The panel's warnings now say plainly what the encryption covers: the page itself is still delivered by your Wi-Fi, Cloudflare or VPS.

## [0.1.7] - 2026-11-08

### New

- `bridge qr <text> [--json]` prints a QR code for any text, so the app can draw the pairing code.
- `bridge telegram token --file <path>` reads the bot token from a file (first line; CRLF and a UTF-8 BOM are accepted).
- `bridge web --json` prints one line with the panel link, the pairing link, the tunnel and the VPS address.

### Fixed

- A kept terminal that runs `h0x-core` now runs this same core, not whichever `h0x-core` is first on PATH.

## [0.1.6] - 2026-11-08

### New

- Remote control (`h0x-core bridge`):
  - Pair your Telegram account and control agents from chat: alerts with Approve, Deny, Show and Interrupt buttons, reply to type into a terminal, and the commands /agents, /read, /send, /keys, /run, /kill and /lock.
  - A web panel for your phone (`bridge web`): QR pairing on the same Wi-Fi (`--lan`), from anywhere through a Cloudflare tunnel (`--tunnel`), or through your own VPS over SSH (`--via`).
  - Safety: one-time pairing codes, a confirm step for risky actions, an audit log, rate limits, secrets hidden in anything sent out, and `bridge lock` as a kill switch.

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
