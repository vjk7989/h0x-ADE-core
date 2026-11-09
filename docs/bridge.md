# Remote control (bridge)

The bridge lets chat accounts you pair drive this machine's terminals while you are away from it.
They can see what your agents are doing, answer their questions, send text and keys, start agents and commands, and stop them.
It is built into core and doesn't depend on any one transport. Telegram is the only one today: a bot you create, run by `h0x-core bridge run`.

[Back to the README](../README.md) · [Security](security.md) · [Commands](commands.md)

## Telegram setup

1. In Telegram, talk to @BotFather: send `/newbot`, pick a name, and copy the token it gives you.
2. Store the token on this machine; it is read from stdin and not shown as you type:

   ```sh
   h0x-core bridge telegram token
   ```

   It is kept in the bridge folder, mode 0600, and never logged or printed: `h0x-core bridge ls` only says whether it is set. `h0x-core bridge telegram token --clear` removes it.
3. Print a pairing code:

   ```sh
   h0x-core bridge pair
   ```

4. Start the bot so it can hear you. It keeps running until stopped; under core it survives your terminal closing:

   ```sh
   h0x-core run --bg --keep -- h0x-core bridge run
   ```

5. In a private chat with your bot, send `/pair CODE` within 10 minutes. The bot answers that you are paired.

The bot only talks to paired accounts in private chats.
Group chats are ignored. Messages from accounts that aren't paired get no answer, except `/pair`.
Messages from other bots and commands addressed to another bot (`/agents@OtherBot`) are ignored too; the bot learns its own name from Telegram when it starts.

It only makes outbound connections: it long-polls `api.telegram.org` over https through the system `curl` and never listens on a port.
The token travels inside the request that curl reads on its stdin, never on a command line that other processes could list.
Errors keep curl's own message but cut the token out of any URL in it, so the token never reaches a log or a chat.
Accounts are known by their numeric Telegram user id; a username is only a label and can't pair or act.
When Telegram can't be reached, it tries again after 1, 2, 4, 8 seconds and so on, up to a minute between tries.

### In the chat

- `/agents`: terminals, with their agent, folder and status
- `/read <t> [lines]`: its last lines (default 20)
- `/send <t> <text>`: type text and press enter
- `/keys <t> <keys…>`: press enter, esc, ctrl+c, up, down, left, right, y, n
- `/approve <t>`, `/deny <t>`: answer what it is waiting on
- `/interrupt <t>`: press its stop key
- `/run <folder> <command>`: start an agent or command (put a folder with spaces in "double quotes")
- `/kill <t>`: end its program
- `/lock`: refuse every remote command until `h0x-core bridge unlock` here
- `/help`: this list

Notices come with buttons: Approve, Deny, Show and Interrupt when an agent needs you; Show for the rest.
To type into a terminal, reply to its notice.
A command that needs a confirmation shows Confirm and Cancel buttons. Cancel leaves the confirmation unused, and it expires in 2 minutes.
Notices stop while the bridge is locked.

## For the app

The desktop app's Settings page drives the same commands; they all print JSON with `--json`.

- `h0x-core bridge qr <text> --json` prints `{"size": N, "rows": ["0101...", ...]}`: N rows of N modules, `1` dark, no quiet zone. Without `--json` it draws the code in the terminal. Empty text, or text longer than a QR code holds, is an error.
- `h0x-core bridge telegram token --file <path>` reads the token from the first line of that file instead of stdin, and checks it the same way. The file is not deleted: the caller does that. It cannot be combined with `--clear`.
- `h0x-core bridge web --json` prints one line once the panel is reachable (after the tunnel's address is known when `--tunnel` is set), then keeps serving: `{"url": "http://ip:port/", "pair": "<link>", "tunnel": "<https address or null>", "via": "<public host or null>", "warning": "<text or null>"}`. `warning` says that the page is delivered by your Wi-Fi, Cloudflare or your VPS (whichever apply), so an attacker who controls that path could change it; it is null when the panel is reachable from this machine only. With a tunnel, `pair` is the link through it; `pair` is null when remote control is locked, and `tunnel` is null if the tunnel could not start. Its text output, the QR code included, goes away. Read the line with `h0x-core read <terminal>` when the panel runs under `h0x-core run --bg --keep`.

## Web panel

`h0x-core bridge web` serves a small web page and a WebSocket API. A phone or an app uses it to see the running agents and control them, the same way the Telegram bot does.

```sh
h0x-core bridge web          # on 127.0.0.1:8377, this machine only
h0x-core bridge web --lan    # on this machine's private network address, for a phone on the same Wi-Fi
h0x-core run --bg --keep -- h0x-core bridge web --lan   # under core, so it keeps running
```

When it starts, it prints the panel's address and a one-time pairing link, as text and as a QR code to scan.
The link is valid for 5 minutes and works once. `h0x-core bridge pair --transport web` prints a new code you can type on the panel instead.
Opening the link pairs the device: it becomes a paired account in `h0x-core bridge ls`, with transport `web` and the device's label, and it receives a session token.
The token stays on the device. This machine keeps only its SHA-256 and compares it in constant time.
Each later connection must give that token as its first message, from the same device (see "Encryption").
`h0x-core bridge unpair <id>` revokes a device at once, and so does a reset.
Everything else is as for Telegram: confirmations, the lock (no notices go out while locked), rate limits, redaction and the audit log.

`--lan` only binds a private address: 10.x, 172.16–31.x, 192.168.x, or 100.64.0.0/10 (Tailscale and other private overlays).
It refuses public, loopback and link-local addresses.
The page comes over plain http, but everything the page and the panel say to each other is encrypted (see "Encryption"). Anyone on that network can still change the page you load, so use it only on a network you trust.
In every mode but localhost the panel prints (and `--json` carries in `warning`) that the messages are end-to-end encrypted but the page itself is delivered by your Wi-Fi, Cloudflare or your VPS, so an attacker who controls that path could change it.

A web session lasts 30 days from pairing; after that the device pairs again. A session saved before this limit existed counts from the first time it is used.
Wrong pairing codes sent over the web are counted per connection source (behind the tunnel, the visitor's address from Cloudflare's `CF-Connecting-IP`, which is believed only on requests that came through the tunnel from this machine): five in a row refuse that source for 10 minutes, before the code is even checked. They never count against Telegram pairing, and Telegram's wrong codes never block web pairing. Failed `auth`s are counted per source the same way and refuse it for a minute.
The panel serves at most 64 connections at once and answers the next with `503`. One source may hold at most 8 connections that have not authenticated yet (behind a proxy, by the visitor's address as above); its ninth gets `503` while other sources are still served, and a connection stops counting toward that share once it pairs or authenticates. A connection that has not paired or authenticated within 30 seconds is closed. An open connection whose session turns 30 days old is closed at the next heartbeat (every 20 seconds). The log of what happened is capped (see "Audit log").

### Ways to reach it

| Way | Command | Who can reach it |
| --- | --- | --- |
| Local | `h0x-core bridge web` | This machine only |
| LAN (plain http) | `h0x-core bridge web --lan` | Devices on your private network |
| Tunnel (Cloudflare) | `h0x-core bridge web --tunnel`, or `h0x-core bridge tunnel on` for a running panel; `bridge tunnel off` closes it | Anyone with the link, through Cloudflare (messages stay end-to-end encrypted; it delivers the page); see "Access from anywhere" |
| Your VPS | `h0x-core bridge web --via vps --public-host h0x.example.com` | Anyone who can reach your VPS's HTTPS name |
| SSH only | `ssh -L 8377:127.0.0.1:8377 you@your-pc`, then open `http://127.0.0.1:8377/` | Only you, with no public exposure at all |

The SSH-only way is for power users: your PC runs `h0x-core bridge web`, and the device you read it from forwards the port with `ssh -L`.
Nothing is exposed to the internet, and the pairing link works through the forwarded port.

### Through your VPS

Add your server once as a machine, then run the panel with `--via`:

```sh
h0x-core machine add you@vps.example.com --name vps
h0x-core bridge web --via vps --public-host h0x.example.com   # --remote-port 8377 by default
```

The bridge keeps an `ssh -N -R` running that opens the port on the server's **loopback only** (`127.0.0.1`, never `0.0.0.0`).
It uses the machine's ssh target, adds `ExitOnForwardFailure=yes`, `ServerAliveInterval=15` and `BatchMode=yes`, and never forwards your ssh agent.
Use an SSH key without a passphrase prompt (or an agent): it cannot ask questions. Connect to the server once by hand first, so its host key is known.
If ssh ends, it is started again with growing pauses (1 s up to 60 s). It is stopped when the bridge stops, and while the bridge is locked.
`--via` needs a plain ssh machine (not one added with `--via` a custom command), a `--public-host` that is a DNS name (no IP address, port or wildcard), and a `--remote-port` from 1024 to 65535.
It cannot be combined with `--lan`.

On the server, put your own HTTPS reverse proxy in front of that port. With Caddy, which gets the certificate for you:

```text
h0x.example.com {
    reverse_proxy 127.0.0.1:8377
}
```

The pairing link and QR code then use `https://h0x.example.com/#pair=...&fp=...`, and that name is added to the accepted `Host` and `Origin` values.
Keep the proxy's `Host` header as it received it (Caddy and nginx's `proxy_set_header Host $host` do), and let it pass WebSocket upgrades.
The panel only sees your proxy (the ssh forward connects from 127.0.0.1), so it takes each visitor's address, for the failure counts and the per-source limit below, from the last hop of `X-Forwarded-For`, or else `X-Real-IP`, on requests for your public host that arrive from loopback. Caddy's `reverse_proxy` sets `X-Forwarded-For`; with nginx add `proxy_set_header X-Forwarded-For $remote_addr;` (or `X-Real-IP`). Without either header every visitor counts as 127.0.0.1, so one visitor's failures would refuse everyone. The headers are ignored in every other mode (LAN, localhost, Cloudflare tunnel).

Security: the proxy on your VPS ends TLS and delivers the page, so it sees only ciphertext but could alter the page; run it only on a server you control.
Pairing and session authentication still apply to every device, and so do the lock, the allow-list, rate limits and the audit log.
Anyone who can reach the name can load the page, so treat a pairing link like a password: it works once, for 5 minutes.

What the server accepts:

- Only its own address (and, with `--via`, your public name) in `Host`, so a hostile web page can't reach it through DNS rebinding.
  A browser's `Origin`, when sent, must be the panel itself. Apps that send no `Origin` still have to authenticate.
- No CORS headers, no request bodies (`GET` only), request heads up to 8 KB that arrive within 10 seconds, and WebSocket messages up to 64 KB.
  Frames must be masked and unfragmented; the first is the client hello (text, at most 1 KB) and every later one a sealed binary frame. Anything else closes the connection.
- A connection must pair or authenticate within 30 seconds and say something at least every 60.
  The server sends a heartbeat every 20 seconds.
- Every answer carries `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'`, `Referrer-Policy: no-referrer` and `X-Content-Type-Options: nosniff`.

### WebSocket messages

Every message is JSON `{"name": ..., "data": ...}`.

From the device:

- `pair` `{token, label}`: the one-time token from the link.
- `auth` `{session}`: every later connection.
- `command` `{cmd, id, ...}`, where `cmd` is one of:
  - `list`
  - `read` `{terminal, lines}`
  - `send` `{terminal, text}`
  - `keys` `{terminal, keys}`
  - `approve`, `deny`, `interrupt`, `kill` `{terminal}`
  - `run` `{folder, command}`
  - `lock`
  - `confirm` `{token}`
- `snapshot`: the list again.
- `ping`: answered with `pong`.

From the server:

- `paired` `{session, device, id}`
- `hello` `{tunnel}`
- `snapshot` `{lines, tunnel}`: the terminals, as `/agents` lists them.
  `tunnel` is true on a connection that came through the Cloudflare tunnel, so the page can say Cloudflare delivers it.
- `reply` `{id, kind, ...}`, where `kind` is one of:
  - `done` `{text}`
  - `needs_confirm` `{token, what}`
  - `locked`
  - `rate_limited` `{retry_secs}`
  - `failed` `{message}`
  - `not_paired`
- `notice` `{terminal, agent, folder, kind, excerpt}`
- `heartbeat`
- `error` `{message}`, after which the connection usually closes.

Reconnecting is the device's job: authenticate again with the session token.

### Encryption

Nothing goes over the WebSocket in the clear. The messages above are the plain content; on the wire each one is sealed.

- The pairing link is `https://host/#pair=CODE&fp=FP`. `FP` is the fingerprint (SHA-256, base64url) of the panel's long-term key. The page pins it the first time and refuses any other.
  The key is made when the panel first starts and kept in the OS secret store as `web.key`, so the fingerprint stays the same across restarts. It is never printed or logged.
  If it is ever damaged, a new one is made and phones must pair again.
- The first frame from the page is a hello (text) with a device key the phone made for itself. The panel answers with its hello. Both keys are mixed with fresh one-time keys, so each connection has its own session keys.
  From then on every frame, both ways, is one binary frame: the JSON message sealed with AES-256-GCM, with a counter that never repeats.
  A text frame after the hello, a frame that does not open, or a binary frame before the hello closes the connection. A plain-text client (before this change) is closed at once and its pairing code is not used up.
- Pairing records the device's key with its session. `auth` only works from that same device key; from any other it fails like a wrong token and counts toward the block.
  Sessions made before this change have no device key and are refused: pair the device again.

The honest limit: this stops anyone who only watches the connection (other devices on the Wi-Fi, the network between, Cloudflare or your VPS proxy as far as the messages go). They see ciphertext.
It does not protect against whoever delivers the page: a Wi-Fi attacker on a plain-http `--lan` page, Cloudflare, or your VPS could hand your phone an altered page that leaks what you type. Use a network you trust for `--lan` and a server you control. The native app will remove this limit, because it carries the code itself.

## Access from anywhere

To reach the panel when the phone is not on your Wi-Fi, switch on a Cloudflare quick tunnel:

```sh
h0x-core bridge web --tunnel             # serve the panel and open a tunnel at once
h0x-core bridge web --tunnel --for 30    # the tunnel stops after 30 minutes (default 60, at most 1440)
h0x-core bridge tunnel on [--for 30]     # open one for a panel that is already running
h0x-core bridge tunnel off               # close it
```

The panel starts `cloudflared tunnel --url http://<panel> --no-autoupdate --protocol http2` and reads its address, `https://<random words>.trycloudflare.com`.
It accepts only an https address with exactly one name before `.trycloudflare.com`.
It prints that address, a warning, and a new one-time pairing link through the tunnel with its QR code; `bridge tunnel on` prints them too.
A new link replaces any earlier one; the newest works.
Only one tunnel runs at a time; `tunnel on` while one is up keeps its address and restarts its timer.

While the tunnel is up, the panel also answers the tunnel's `Host`, and a browser's `Origin` must then be the tunnel's own https page.
Everything else stays the same: pairing, session tokens, the lock, rate limits, redaction and the audit log.

What passes through Cloudflare: the connection is https from the phone to Cloudflare, and Cloudflare hands it on to cloudflared here. The messages inside are end-to-end encrypted (see "Encryption"): Cloudflare reads only ciphertext.
It sees the address, the page it delivers, and the size and timing of the messages. The pairing code and the key fingerprint sit after the `#` of the link, which a browser never sends.
Cloudflare also delivers the page, so it could alter it to read your session; the panel tells the device it is behind the tunnel (`tunnel: true` in `hello` and `snapshot`).
Anyone who learns the address can reach the page, but not control anything without pairing.

The tunnel stops, and cloudflared is killed:

- when its time is up (`--for`, 60 minutes by default);
- on `h0x-core bridge tunnel off`;
- within a second of a lock, from a device or with `h0x-core bridge lock`; `tunnel on` is refused while locked;
- if cloudflared ends by itself;
- when the panel stops: Ctrl+C, an error, or its core terminal being killed (on Windows the terminal's job ends cloudflared with it).

If the panel itself is killed some other way, cloudflared may keep running for a while, but the address is no longer accepted: a new panel only answers its own tunnel.

`bridge tunnel on|off` finds the running panel through the bridge folder: it writes `tunnel.request`, which the panel takes within a second, and reads the panel's answer from `tunnel.json`.
With no panel running, it says so after 3 seconds.

### cloudflared

A `cloudflared` on `PATH` is used first. Otherwise the panel uses one downloaded beside `h0x-core`.
If there is none, `bridge web --tunnel` and `bridge tunnel on` ask before downloading one (pass `--yes` to skip the question; without a terminal and without `--yes` they refuse).
The download is a pinned cloudflared release (2026.10.7) from github.com/cloudflare/cloudflared, about 20–55 MB depending on the platform, fetched with the system `curl` over https only.
It is installed only if its SHA-256 matches the one pinned in core; otherwise it is deleted and refused.
The downloaded copy is hashed again before every start and refused if it no longer matches (Windows and Linux; the macOS download is pinned as an archive, so the unpacked program has nothing to compare). A `cloudflared` on `PATH` is yours and is not checked.
Pinned builds: Windows x64, Linux x64 and arm64, macOS x64 and arm64 (unpacked with `tar`). Elsewhere, install cloudflared yourself.

## Full control means full control

A paired account has the same power as you at this machine's keyboard, through core:

- list terminals with their agent, folder and status, and read the last lines of any of them;
- type text into a terminal, press keys (enter, esc, ctrl+c, arrows, y, n), and interrupt;
- approve or deny what an agent is waiting on;
- start an agent, a template or any shell command in any folder;
- kill terminals.

Anything your account can do, a shell started this way can do too: read and change files, use your credentials, reach your network.
Pair only accounts you control, on a chat service you trust with that.
Core is not a sandbox (see [security](security.md)), and the bridge doesn't make it one.

## Pairing

```sh
h0x-core bridge pair
```

prints a one-time code: 8 characters from the operating system's random source, valid for 10 minutes, usable once.
A new code replaces the previous one.
The account that sends the code to the bridge is paired: it is added to the allow-list (`h0x-core bridge ls`) with its transport, its id there, a label and the time.
Messages from any other account are refused without reaching core.

Five wrong codes in a row disable chat pairing for 10 minutes and throw the live chat code away, so guessing a code doesn't work. (A live web link is left alone, and wrong codes over the web are limited per source instead; see "Web panel".)

```sh
h0x-core bridge ls              # paired accounts, and whether the bridge is locked
h0x-core bridge unpair 2        # remove one (its id from ls)
h0x-core bridge unpair --all    # remove every one
```

Unpairing takes effect on the next message.

## Confirmations

Some commands don't run until the same account confirms them.
The bridge answers with a token, and nothing has run yet.
Confirming with that token within 2 minutes runs exactly what was described. A token works once, and only for the account it was given to.
These commands need a confirmation:

- kill;
- run (start an agent or command);
- text, keys, approve or deny sent to a program that doesn't report its status over OSC 7501, such as a plain shell, where typed text runs as a command.

Text and answers sent to an agent that reports its status run at once.
Interrupt never needs a confirmation, even into a plain shell: it only presses the stop key (esc for a program that reports its status, else ctrl+c), and a stop that has to wait two minutes is no stop.

A waiting confirmation is void once the account is unpaired or paired again, and after any lock, whether it was locked from a chat or with `h0x-core bridge lock` here. Unlocking doesn't bring it back; ask again.

Approve and deny press keys for you. For a program that reports its status and is `blocked`, approve presses enter and deny presses esc.
In agents' permission dialogs, enter takes the highlighted first choice ("yes") and esc cancels.
A reporting program that isn't blocked gets nothing, and the reply says what it is doing.
For any other program, approve types `y` and enter, and deny types `n` and enter, after a confirmation.
The bridge has no list of agents and reads no screens. It goes only by what the program reports, like the rest of core.

## Lock

```sh
h0x-core bridge lock
h0x-core bridge unlock
```

While locked, the bridge refuses every remote command, including confirmations already given, and refuses pairing.
A paired account can lock the bridge from a chat, for example when a phone is lost.
Only `h0x-core bridge unlock` on this machine lifts the lock; no remote command can.
Locking voids every confirmation that was waiting.

## Limits

Each account may send 30 commands a minute; beyond that, the reply says how long to wait.
Notices (it needs you, it's done, it hit an error, it finished) come at most once per terminal every 10 seconds.
If an agent goes back to work while a notice is held back, the notice is dropped.
Both limits are set in config.toml; `notice_quiet` is capped at an hour:

```toml
[bridge]
rate_limit = 30      # commands per account per minute
notice_quiet = 10    # seconds between notices about one terminal
```

## Redaction

Text that leaves this machine is redacted first: terminal output, terminal lists, notices, error messages and confirmation prompts.
Common secret shapes become `[redacted]`:

- API keys and tokens: `sk-…`, `ghp_…` and other GitHub tokens, Slack `xox…-` tokens, AWS `AKIA…` keys;
- `Bearer` tokens, whatever whitespace follows the word;
- private key blocks, from -----BEGIN … KEY----- through their END line;
- long values after names ending in token, secret, password or key (`API_KEY=…`, `"auth_token": "…"`).

Redaction matches known shapes and nothing else, so it is a safety net, not a guarantee.
A secret in an unusual shape still goes out, and so does one split across lines (a wrapped key, a value on the line after its name): each line is matched on its own terms. Don't read terminals that print secrets you can't afford to send to the chat service.

## Audit log

Every command that reaches the bridge is appended to `audit.jsonl`, and so is every pairing attempt.
Each entry has the time, the account, the command, the terminal and the result.
Typed text, commands, folders and terminal names are stored redacted and cut to 24 characters; text keeps its length.
Pairing codes and confirmation tokens are never stored: a confirmation is logged as `confirm` with the action it ran.
If the log can't be written, the command doesn't run.
Failed or unauthenticated attempts (wrong pairing codes, failed `auth`) are written verbatim only for the first five of each minute; the rest of that minute becomes one line, such as `245 more failed attempts from 245 sources in the last minute`, so an attacker rotating sources cannot push real entries out of the log. Authenticated commands are always written verbatim.
When `audit.jsonl` reaches 1 MiB it is moved to `audit.jsonl.1`, replacing the older one, so the log keeps between 1 and 2 MiB of the latest entries.

```sh
h0x-core bridge log         # the last 20 entries
h0x-core bridge log -n 100
```

## Files

The bridge folder is `bridge/` beside config.toml: `~/.config/h0x-core/bridge/`, or `%APPDATA%\h0x-core\bridge\` on Windows.
It holds `state.json` (paired accounts, the live code, the lock) and `audit.jsonl`, and while a web panel runs, `tunnel.request` and `tunnel.json` (the tunnel's address and its pairing link).
On Unix, the folder is 0700 and both files are 0600. On Windows, they use the folder's permissions, like the rest of core's state.
Pairing codes sit in `state.json` for their 10 minutes. Treat that folder like an SSH key.

If `state.json` is damaged (not a JSON object of the right shape), the bridge refuses every command and pairing until you run

```sh
h0x-core bridge reset
```

on this machine. It moves the file aside as `state.json.bad-<time>` and starts empty: no paired accounts, unlocked. Pair again afterwards. No remote command can reset.

## Threat model

The bridge defends against:

- **Someone else messaging the bot.** Only paired accounts get through.
- **Guessing a pairing code.** A code has 40 random bits and lives 10 minutes; five wrong guesses stop pairing for 10 minutes.
- **A slip of the thumb.** Destructive commands wait for a confirmation that works once.
- **A flood of commands from a paired account.** Each account is held to 30 a minute.
- **A lost or stolen phone.** Lock from any paired account, or unpair it here. Unlocking takes this machine.
- **Secrets leaking into chat history.** Known secret shapes are redacted before anything is sent.
- **Not knowing what happened.** The audit log records every remote command.
- **Web pages reaching the web panel.** Strict `Host` and `Origin` checks stop DNS rebinding and other sites' pages, and the panel's own CSP keeps it from loading anything from elsewhere.
- **A leaked device database.** Session tokens are stored only as SHA-256 hashes.

It does not defend against:

- **A paired account that is compromised.** It has full control until you unpair it or lock.
- **The chat service itself.** It sees everything sent through it.
- **Other programs running as your account.** They can already reach core and its files.
- **Secrets the redaction patterns don't recognize.**
- **Someone on a `--lan` network who alters the page.** The page is plain http there; the messages are encrypted, but whoever changes the page could read them (see "Encryption").
- **Cloudflare or your VPS proxy altering the page.** They deliver the page, so a changed page could read the messages; they cannot read them without doing that.

The Telegram bot only makes outbound connections.
The web panel listens on 127.0.0.1, or on a private LAN address with `--lan`, never on a public one.
With the tunnel on, cloudflared makes an outbound connection to Cloudflare and passes requests on to the panel.
