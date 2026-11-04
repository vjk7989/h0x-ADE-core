# Machines

Run a program on another machine and come back to it from here.
The program lives there. Losing your connection does not end it.

[Back to the README](../README.md) · [Command reference](commands.md#h0x-core-machine)

## Add a machine over SSH

Use the same target you would give SSH, including a Host from your SSH config:

```sh
h0x-core machine add user@host --name box
h0x-core machine ls
h0x-core run --on box --cwd /srv/app cargo test
```

The other machine needs h0x-core. In an interactive terminal, add asks before copying h0x-core there if it is missing or too old to talk to.
For a script, `h0x-core machine add user@host --name box --install` allows that copy without asking.
When the OS and CPU match, it copies your current build. Otherwise it uses this version's released build for that target.

SSH passwords, codes, and host-key prompts need a person at a terminal.
On Windows, use SSH keys or an SSH agent; h0x-core does not share interactive password logins there.

To reach a separate core session on the same host:

```sh
h0x-core machine add user@host --name sandbox --session scratch
```

Sessions separate terminals and state, not permissions. See [security](security.md).

## Use another way in

A custom connection command receives a shell line as its last argument.
It must carry stdin and stdout unchanged and ask no questions:

```sh
h0x-core machine add --name box --via 'ssh -p 2222 -i /home/me/.ssh/key user@host'
h0x-core machine add --name sandbox --temp --via 'fly ssh console -a my-app --pty=false -C'
```

The command runs on your local machine. The shell line runs on the destination and connects to h0x-core there.
Log in with that tool first if it needs credentials.
The temporary marker only helps clients show short-lived machines apart; it does not delete anything for you.

### Vercel Sandbox

The `sandbox` CLI does not carry stdin as a plain pipe, so it needs a small helper:
[`via/vercel-sandbox.py`](via/vercel-sandbox.py) (Python 3, macOS or Linux).
Log in with `sandbox login` first, and give the sandbox a timeout long enough for the work:
when it stops, its terminals stop with it.

```sh
sandbox create --name my-box --timeout 1h
h0x-core machine add --name my-box --temp --install --via '/path/to/vercel-sandbox.py my-box'
h0x-core run --bg --on my-box --cwd /vercel/app npm run build
```

## What runs where

Your local core keeps the machine connection and combines terminal lists and events.
The remote core owns its programs, terminal history, and files.
A local name such as `box` is your name for that connection, not a remote hostname requirement.

```sh
h0x-core run --bg --on box --cwd /srv/app --name build cargo build
h0x-core ls
h0x-core read box/build --lines 100
h0x-core attach box/build
```

A returned address such as `box/t3` points to a terminal on box.
Use the full address when sending, reading, waiting, or attaching from here.
Ctrl+\ leaves an attached terminal without ending its program.

Remote paths start at the remote user's home unless you set a working directory.
Local paths start at your current folder. Prefer explicit remote paths.

## Commands and files without a terminal

Use exec for a short command whose exact output you need:

```sh
h0x-core exec --on box --cwd /srv/app git diff
printf 'hello\n' | h0x-core exec -i --on box cat
```

Use file for remote reads and exact edits:

```sh
h0x-core file read --on box /srv/app/src/main.rs --offset 1 --limit 100
h0x-core file edit --on box /srv/app/config.txt --old 'port=3000' --new 'port=4000'
h0x-core file write --on box /srv/app/note.txt < note.txt
```

Write replaces the whole file and creates parent folders.
Edit requires the old text exactly once. Add context if it is missing or repeated.
Repeat old/new pairs for several edits; all succeed or none do.
`h0x-core put box/build ./report.txt` sends a local file beside that terminal and prints where it landed.

## When something goes wrong

- Check `h0x-core machine ls` first. It shows the connection state and the last observed h0x-core version.
- If SSH needs a password or code again, run `h0x-core machine login box` in a terminal. On Windows, fix the keys first and use login to reconnect.
- If h0x-core is missing or incompatible, use `h0x-core machine update box` to copy this h0x-core there and restart that core. Its terminals keep running.
- If the connection closes at once, check that the remote login prints no extra text to stdout. The connection carries a byte protocol, not a terminal session.
- With a custom connection, check that it passes bytes unchanged and does not prompt. Do not request a remote PTY.
- If a socket path is too long, use a shorter session name or socket path. See [state and endpoints](security.md#state-and-endpoints).

Once a machine is added, core retries a lost connection. Reattach after it returns.
To forget the connection here, use `h0x-core machine rm box`. That does not stop its remote terminals.
