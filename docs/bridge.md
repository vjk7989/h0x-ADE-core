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

It only makes outbound connections: it long-polls `api.telegram.org` over https through the system `curl` and never listens on a port.
The token travels inside the request that curl reads on its stdin, never on a command line that other processes could list.
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

Five wrong codes in a row disable pairing for 10 minutes and throw the live code away, so guessing a code doesn't work.

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

```sh
h0x-core bridge log         # the last 20 entries
h0x-core bridge log -n 100
```

## Files

The bridge folder is `bridge/` beside config.toml: `~/.config/h0x-core/bridge/`, or `%APPDATA%\h0x-core\bridge\` on Windows.
It holds `state.json` (paired accounts, the live code, the lock) and `audit.jsonl`.
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

It does not defend against:

- **A paired account that is compromised.** It has full control until you unpair it or lock.
- **The chat service itself.** It sees everything sent through it.
- **Other programs running as your account.** They can already reach core and its files.
- **Secrets the redaction patterns don't recognize.**

The bridge only makes outbound connections and never listens on a network port.
