# riverctl

Command-line interface for [River](https://github.com/freenet/river), a decentralized group chat built on [Freenet](https://freenet.org). `riverctl` lets you use River from the terminal: useful for scripting, power users, and headless servers.

## Prerequisites

* [Rust toolchain](https://rustup.rs/)
* A running [Freenet peer](https://freenet.org/quickstart/)

## Install

```bash
cargo install riverctl
```

## First steps

Accept an invite to an existing room (the easiest way to start). Click **Get Invite Code** on the [Freenet quickstart page](https://freenet.org/quickstart/) and expand "Using riverctl?" to copy the invite string, then:

```bash
riverctl invite accept <invite-code>
riverctl room list
```

Or create your own room:

```bash
riverctl room create --name "My Room" --nickname "Alice"
riverctl room list    # Copy the Room Owner VK from the output.
```

## Sending and receiving messages

```bash
riverctl message send   <room-owner-vk> "Hello, River!"
riverctl message list   <room-owner-vk>        # Recent history.
riverctl message stream <room-owner-vk>        # Live stream, Ctrl-C to stop.
riverctl message reply  <room-owner-vk> <message-id> "Thread reply."
riverctl message react  <room-owner-vk> <message-id> 👍
riverctl message edit   <room-owner-vk> <message-id> "Fixed typo."
riverctl message delete <room-owner-vk> <message-id>
```

## Inviting others

```bash
riverctl invite create <room-owner-vk>           # Prints an invite code.
riverctl invite accept <invite-code>             # On the recipient's machine.
```

## Managing your identity

Each room uses a separate signing key, so there is no single global member ID —
`whoami` is per-room:

```bash
riverctl identity whoami <room-owner-vk>         # Your member ID in one room.
riverctl identity whoami                         # Every room you're in.
```

The `member_id` it reports is exactly the top-level `author` value your own
messages carry in `message list` / `message stream --format json`, which is what
a bridge needs to filter out its own echo:

```bash
me=$(riverctl identity whoami <room-owner-vk> --format json | jq -r .member_id)
riverctl message stream <room-owner-vk> --format json --no-version-check |
  jq -c --arg me "$me" 'select(.author != $me)'
```

(The `author` inside `reply_to` is a display nickname, not a member ID — only
the top-level `author` is comparable.)

`whoami` needs no node: it resolves from local storage, so it works with the
peer stopped and before any message has arrived. It does need a *writable*
config dir, since the shared storage loader may rewrite `rooms.json` when the
bundled contract WASM has changed. Pass `--no-version-check` (or set
`RIVERCTL_NO_VERSION_CHECK`) if you poll it, so it never makes the once-a-day
crates.io version request.

It reports the identity that will actually **sign**, across all three override
mechanisms, using the same precedence `message send` does — inline
`--signing-key` / `RIVER_SIGNING_KEY` beats `--signing-key-file` /
`RIVER_SIGNING_KEY_FILE`, which beats the per-room key in `rooms.json`. The
winner is reported as `signing_key_source` (`inline` / `override` / `stored`).
With `--signing-key` the room need not be in local storage at all, matching
`message send --signing-key`:

```bash
RIVER_SIGNING_KEY=<base64-key> riverctl identity whoami <room-owner-vk> --format json
```

`room list --format json` carries the same `self_member_id` (plus
`signing_key_source`), so one call covers every room. Note that a
`--signing-key-file` override applies to *every* room, including ones that
identity is not a member of.

**On comparing IDs.** A `MemberId` renders as 8 base32 characters — a truncated,
non-cryptographic hash of the verifying key. That is fine for recognising your
own messages, which is what it is for. Do not treat it as a security boundary:
it is short enough to collide deliberately, so don't grant trust based on an
`author` match alone.

Export your identity to move between machines or back it up:

```bash
riverctl identity export <room-owner-vk> > my-identity.token
riverctl identity import < my-identity.token     # On another machine.
```

## Member management

```bash
riverctl member list         <room-owner-vk>
riverctl member set-nickname <room-owner-vk> "New Nickname"
riverctl member ban          <room-owner-vk> <member-vk>   # Owner only.
```

## Command reference

| Group      | Commands                                                                |
|------------|-------------------------------------------------------------------------|
| `room`     | `create`, `list`, `join`, `leave`, `republish`, `config`                |
| `message`  | `send`, `list`, `stream`, `edit`, `delete`, `react`, `unreact`, `reply` |
| `member`   | `list`, `set-nickname`, `ban`                                           |
| `invite`   | `create`, `accept`                                                      |
| `dm`       | `send`, `list`, `purge`, `accept`                                       |
| `identity` | `whoami`, `export`, `import`                                            |
| `debug`    | troubleshooting utilities                                               |

Run `riverctl <group> --help` or `riverctl <group> <cmd> --help` for full flags. All commands accept `--format json` for scripting.

### `message stream --format json` event types

`message stream` emits one JSON object per line (JSONL). Each carries a `type`:

| `type` | Meaning | Fields |
|--------|---------|--------|
| `message` | A new message | `message_id`, `room`, `author`, `nickname`, `content`, `timestamp`, `edited`, `reply_to`, `reactions` |
| `edit` | A previously-streamed message's content changed | same as `message` (with the new `content` and `edited: true`) |
| `delete` | A previously-streamed message was deleted | `message_id`, `room`, `author`, `nickname`, `timestamp` (no `content`) |

`reply_to` is `null` for non-replies, otherwise `{ "author", "preview" }`. `edit`/`delete` events are emitted only for messages the stream actually surfaced. Bridges should key off `type` and tolerate unknown future types.

The top-level `author` is the sending member's ID; get your own with `riverctl
identity whoami <room-owner-vk>` and compare against it to recognise your own
messages. (`reply_to.author` is a display nickname, not an ID.)

## Configuration

- `--node-url <URL>`: override the Freenet node URL (default `ws://127.0.0.1:7509/...`).
- `--config-dir <PATH>`: override where `riverctl` stores room data and signing keys (default follows `XDG_CONFIG_HOME` conventions).
- `--log-file <PATH>`: write logs to a file instead of stderr (stdout is reserved for command output).
- `RIVERCTL_LOG_FILE` env var: same as `--log-file`.

## Links

- [River repository](https://github.com/freenet/river)
- [Freenet website](https://freenet.org)
- [Freenet quickstart](https://freenet.org/quickstart/)

## For contributors

If you change `common/`, `contracts/`, or `delegates/` code and rebuild WASM, you must keep the CLI's bundled copy in sync:

```bash
cargo make sync-cli-wasm
```

The CLI build double-checks and panics if the bundled WASM drifts from the most recently built artifact. See the top-level `AGENTS.md` ("Delegate & Contract WASM Migration") for the full migration workflow.

An integration smoke test lives at `tests/message_flow.rs`. It is `#[ignore]` by default; run it manually with:

```bash
cargo test --test message_flow -- --ignored --nocapture
```

Prerequisite: `~/code/freenet/freenet-core/main` must exist (the harness builds the Freenet binary from there).
