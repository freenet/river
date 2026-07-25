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

## Direct messages

End-to-end-encrypted one-to-one messages between two members of the same room.
They travel inside the room's contract state, so both people must already be
members — there is no cross-room DM.

The recipient is a member ID: either the short 8-character form that
`riverctl member list` prints, or the full ID.

```bash
riverctl member list <room-owner-vk>             # Find the recipient's member ID.
riverctl dm send  <room-owner-vk> <recipient> "Hello, just between us."
riverctl dm list  <room-owner-vk>                # Threads to/from you, decrypted.
riverctl dm purge <room-owner-vk> <purge-token>  # Drop a DM addressed to you.
```

The purge token is the 32-character hex value `dm list` prints beneath each
inbound DM as `purge token: …`.

Only the recipient can decrypt a DM, so `dm list` renders your own sent
messages from a local plaintext cache. A DM you sent from a different machine
shows as ciphertext-only there.

### Inviting someone via DM

You can hand a room invitation to a co-member *as a DM*. The recipient's River
UI renders it as a clickable **Invitation card** with an Accept button — unlike a
bare invite code pasted into `dm send`, which just shows up as raw text.

```bash
# Invite <recipient> (a member of the carrier room) to <target-room>.
riverctl dm invite <carrier-room-vk> <recipient> --room <target-room-vk> -m "come join!"
```

- `<carrier-room-vk>` is the room the DM travels in — you and the recipient must
  both be members of it.
- `--room <target-room-vk>` is the room you are inviting them to. It must be a
  *different* room you're a member of (the recipient is already in the carrier
  room). A fresh single-use invite credential is minted, exactly as
  `invite create` does.
- `-m/--message` is an optional note shown above the Accept button.

The recipient accepts with `dm accept` (below), or clicks the card in the UI.

### Accepting an invitation that arrived as a DM

`dm list` shows an invite DM as `[Invitation to room …]`. Accept it with:

```bash
riverctl dm accept <carrier-room-vk>
```

`<carrier-room-vk>` is the room whose DM thread **contains** the invitation, not
the room you are joining. If you have invite DMs for several rooms, narrow with
`--from <sender>` or `--room <target-room>`.

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
`signing_key_source`) and honours the same `--signing-key` / `RIVER_SIGNING_KEY`,
so one call covers every room and always agrees with `whoami`. Note that an
override applies to *every* room, including ones that identity is not a member
of — which is what `signing_key_source` is there to tell you.

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
riverctl member ban          <room-owner-vk> <member-id>
```

`member ban` is not owner-only: the room owner can ban anyone, and so can a
member banning within their own invite subtree, or a deputy of such a member
(see below).

### Deputies

A deputy can ban within their deputizer's invite subtree. Deputies are
**per-deputizer**, not a room-wide flag: "is X a deputy?" is only meaningful as
"whose deputy is X?", so both queries name the deputizer explicitly.

```bash
riverctl member deputize      <room-owner-vk> <member-id>  # Grant.
riverctl member revoke-deputy <room-owner-vk> <member-id>  # Revoke.

riverctl member deputies      <room-owner-vk> [member-id]  # Who has X deputized?
riverctl member deputized-by  <room-owner-vk> <member-id>  # Who has deputized X?
```

`member deputies` defaults to your own identity in the room (the identity that
would sign, so `--signing-key-file` applies).
`member deputized-by` is the one that answers "does this member have moderation
authority, and from whom?" The room owner's grant covers every member; anyone
else's covers only the members they invited, directly or indirectly, which is
often nobody.

Both accept `--format json`, emitting `{room_id, direction, subject, grants[]}`.
Each grant carries `deputizer`, `deputy`, `scope` (`room-wide` /
`invite-subtree`), `active`, and `reaches_members`.

`reaches_members` is how many members the grant can currently be used against,
computed from the contract's own rules: `0` for an inactive grant, every member
for an owner grant, otherwise the deputizer's invite subtree minus anyone in it
who has themselves deputized this deputy (a member cannot be banned by someone
they deputized). It is often `0` even for a live grant, because most members
have invited nobody. `reaches_members > 0` is the check for "this member
actually has moderation authority here".

A grant whose deputizer or deputy has left the room is reported with
`active: false`; the contract does not honour such a grant. `in_room` is `true`
for the room owner even though the owner never appears in the member list.

Nicknames are quoted and escaped in terminal output. They are set by the member
themselves and are printed next to an authority claim, so an unescaped one could
forge a line that reads as a real deputy grant. The `-f json` output carries the
faithful nickname.

`member list --format json` gains `deputies` (who this member deputized) and
`deputized_by` (who deputized them) alongside the existing `member_id` and
`nickname`. `debug room-state` gains `deputy_grant_count` and `deputy_grants`.

Two caveats worth knowing:

- The read commands reject an ambiguous partial member ID and list the matches;
  the write commands (`ban`, `deputize`, `revoke-deputy`) still resolve a
  partial ID to the first match. Pass the full 8-character ID to be certain.
- Like every riverctl command that reads a room, these fetch through the
  standard path, which may PUT migrated state or publish a `member_info` heal
  for your own identity. They never write deputy state.

## Command reference

| Group      | Commands                                                                |
|------------|-------------------------------------------------------------------------|
| `room`     | `create`, `list`, `join`, `leave`, `republish`, `config`                |
| `message`  | `send`, `list`, `stream`, `edit`, `delete`, `react`, `unreact`, `reply` |
| `member`   | `list`, `set-nickname`, `ban`, `deputize`, `revoke-deputy`, `deputies`, `deputized-by` |
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
