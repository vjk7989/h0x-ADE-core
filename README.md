# HyperOx core (h0x-core)

`h0x-core` is a headless terminal runtime for coding agents. It runs programs
and agents in terminals that stay alive after you leave; agents can start other
agents, send them work, and wait on them; you can attach to any terminal and
take over. It draws nothing: use it from your shell, an agent, or your own app.

## Install

Linux and macOS:

```sh
curl -fsSL https://github.com/vjk7989/h0x-ADE-core/releases/latest/download/install.sh | sh
```

Windows (PowerShell):

```powershell
powershell -c "irm https://github.com/vjk7989/h0x-ADE-core/releases/latest/download/install.ps1 | iex"
```

Or download a release asset for your platform (Linux and macOS on x86_64 and
aarch64, Windows on x86_64) and put `h0x-core` on your PATH. Linux builds are
static. On Windows, templates and quoted shell commands need `sh` on PATH, such
as Git for Windows provides.

Run `h0x-core update` to update. Running terminals keep running.
See [security](docs/security.md) for the trust and signing details.

## Quick start

```sh
h0x-core run cargo test                  # stream output and wait for it
h0x-core run --bg --name web bun dev     # start it and return
h0x-core ls                              # see live terminals
h0x-core attach web                      # take over; Ctrl+\ detaches
h0x-core read web --lines 100            # read without attaching
```

Closing your viewer does not end the program. A waiting run checks in after
180 seconds by default, leaving the program running and printing the terminal's
address so you can read it or wait again.

For a terminal you want to keep even if its program is lost, use `h0x-core run --keep`.
Recovery starts a fresh shell, never reruns the task. A normal exit or an
explicit kill still ends it.

## Default shell

With no program, `h0x-core run --bg` starts the machine's shell: `$SHELL` (or
`/bin/sh`) on Unix, PowerShell 7 on Windows if available, else Windows PowerShell.
Override it in `~/.config/h0x-core/config.toml` (Windows:
`%APPDATA%\h0x-core\config.toml`):

```toml
shell = ["nu"]
```

The list is an executable followed by separate arguments. Changes apply to the
next new shell or recovery; running terminals keep their behavior.

## Agents run agents

Give an agent `h0x-core --skill`. It prints the agent manual in one read:
how to delegate, wait, report back, and clean up.

[Templates](docs/templates.md) give an agent a command and a role:

```sh
h0x-core templates
h0x-core run --bg --name review reviewer "Review src/x.rs. Read-only."
h0x-core send review "Focus on the error path."
h0x-core read review --lines 100
```

Agents started inside an `h0x-core` terminal become its children. They report
back with `h0x-core send`; the parent waits with `h0x-core wait --children all`
and gets a note when a child needs attention or ends. Children end with their
parent unless started with `--outlive-parent`.

## Machines

```sh
h0x-core machine add my-ssh-host --name box
h0x-core run --on box --cwd /srv/app cargo test
h0x-core attach box/t3
```

Use the address your run returns, not the example's `box/t3`. Programs and
files stay on that machine; your local core lists its terminals beside yours.
[Machines](docs/machines.md) covers SSH, custom connections, remote files, and reconnecting.

## Status comes from the program

A program can say it is working, done, blocked, or in error through OSC 7501,
the Program Status Protocol. Core stores what it reports and never guesses from
screen text. A program that does not report status has none.
Use `h0x-core get review` for a snapshot or `h0x-core events` to follow changes.

## Build your own client

`h0x-core schema` prints the request/response API as OpenRPC with JSON Schema.
[The API docs](schema/README.md) cover connecting, terminal streams, events, and status.
`h0x-core connect` carries the protocol over stdin and stdout, including through SSH.

## Part of HyperOx

The HyperOx desktop app, [`h0x-ade`](https://github.com/vjk7989/h0x-ADE-app),
and the HyperOx terminal runtime, [`h0x`](https://github.com/vjk7989/h0x-ADE-Runtime),
both run on this core.

## Docs

- [Commands](docs/commands.md): generated from the binary's help.
- [Machines](docs/machines.md): work across machines.
- [Templates](docs/templates.md): reusable agent roles.
- [Security](docs/security.md): access, state, and updates.
- [Bridge](docs/bridge.md): remote control from paired chat accounts.
- [API](schema/README.md): build a client.

For a quick lookup, run `h0x-core --help` or a command's help, such as `h0x-core run --help`.

## Build from source

You need Rust 1.95 or newer and Zig 0.16.0.

```sh
git clone --recurse-submodules https://github.com/vjk7989/h0x-ADE-core
cd h0x-ADE-core
cargo build --release
cargo test --locked
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contributor setup and checks.

## License

[Apache-2.0](LICENSE); see [NOTICE](NOTICE) for attribution.
`h0x-core --licenses` prints the included third-party licenses.
