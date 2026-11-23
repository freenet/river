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
members. There is no cross-room DM.

The recipient is the short 8-character ID or the full verifying key that
`riverctl member list` prints. The full key names them unambiguously
([Member IDs and verifying keys](#member-ids-and-verifying-keys)).

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

You can hand a room invitation to a co-member as a DM. The recipient's River
UI renders it as a clickable Invitation card with an Accept button. A bare
invite code pasted into `dm send` shows up as raw text instead.

```bash
# Invite <recipient> (a member of the carrier room) to <target-room>.
riverctl dm invite <carrier-room-vk> <recipient> --room <target-room-vk> -m "come join!"
```

- `<carrier-room-vk>` is the room the DM travels in. You and the recipient must
  both be members of it.
- `--room <target-room-vk>` is the room you are inviting them to. It must be a
  different room you're a member of (the recipient is already in the carrier
  room). riverctl mints a fresh single-use invite credential, exactly as
  `invite create` does.
- `-m/--message` is an optional note shown above the Accept button.

The recipient accepts with `dm accept` (below), or clicks the card in the UI.

### Accepting an invitation that arrived as a DM

`dm list` shows an invite DM as `[Invitation to room …]`. Accept it with:

```bash
riverctl dm accept <carrier-room-vk>
```

`<carrier-room-vk>` is the room whose DM thread holds the invitation, not
the room you are joining. If you have invite DMs for several rooms, narrow with
`--from <sender>` or `--room <target-room>`.

## Inviting others

```bash
riverctl invite create <room-owner-vk>           # Prints an invite code.
riverctl invite accept <invite-code>             # On the recipient's machine.
```

## Managing your identity

Each room uses a separate signing key, so there is no single global member ID.
`whoami` is per-room:

```bash
riverctl identity whoami <room-owner-vk>         # Your member ID in one room.
riverctl identity whoami                         # Every room you're in.
```

`whoami` reports your member ID and your verifying key
([Member IDs and verifying keys](#member-ids-and-verifying-keys)). A bridge
filtering out its own echo should use the full key:

```bash
me=$(riverctl identity whoami <room-owner-vk> --format json | jq -r .verifying_key)
riverctl message stream <room-owner-vk> --format json --no-version-check |
  jq -c --arg me "$me" 'select(.author_verifying_key != $me)'
```

A pipeline's exit status is its LAST command's, so a wrapper around this one
sees `jq`'s status, not riverctl's. Use `set -o pipefail` in bash, or restart on
any exit. For a bot that runs unattended, see
[Running `message stream` as a long-lived bot](#running-message-stream-as-a-long-lived-bot).

`whoami` needs no node: it resolves from local storage, so it works with the
peer stopped and before any message has arrived. It does need a *writable*
config dir, since the shared storage loader may rewrite `rooms.json` when the
bundled contract WASM has changed. Pass `--no-version-check` (or set
`RIVERCTL_NO_VERSION_CHECK`) if you poll it, so it never makes the once-a-day
crates.io version request.

`whoami` reports the identity that will actually sign, using the same
precedence as `message send`. It names the winner in `signing_key_source`:

| Source | Flag or env | `signing_key_source` | Wins over |
|---|---|---|---|
| Inline key | `--signing-key` / `RIVER_SIGNING_KEY` | `inline` | `override`, `stored` |
| Key file | `--signing-key-file` / `RIVER_SIGNING_KEY_FILE` | `override` | `stored` |
| Per-room key in `rooms.json` | none | `stored` | nothing |

With `--signing-key` the room need not be in local storage at all, matching
`message send --signing-key`:

```bash
RIVER_SIGNING_KEY=<base64-key> riverctl identity whoami <room-owner-vk> --format json
```

`room list --format json` carries the same `self_member_id` (plus
`signing_key_source`) and honours the same `--signing-key` / `RIVER_SIGNING_KEY`,
so one call covers every room and always agrees with `whoami`. An override
applies to *every* room, including rooms that identity is not a member of.
`signing_key_source` shows when one is in effect.

Export your identity to move between machines or back it up:

```bash
riverctl identity export <room-owner-vk> > my-identity.token
riverctl identity import < my-identity.token     # On another machine.
```

## Member IDs and verifying keys

A member has two identifiers:

- The member ID (`MemberId`): 8 base32 characters, a 40-bit truncation of a
  64-bit non-cryptographic hash of the verifying key. Two members can share
  one, and it is short enough to collide deliberately.
- The verifying key: the member's full base58 ed25519 key. It names exactly one
  member.

Compare each `whoami` field only against fields of the same kind:

| `whoami` field | Compares against | Use for |
|---|---|---|
| `member_id` | An event's top-level `author`; `member list`'s `member_id`; `room list`'s `self_member_id` | Recognising your own messages |
| `verifying_key` | An event's `author_verifying_key`; `member list`'s `verifying_key` | Echo filters, and any decision about who to trust |

**Never cross them.** `member_id` compared against `author_verifying_key` is
never equal, which silently disables an echo filter and can put a bridge in a
feedback loop.

**Never grant trust on a member ID match alone.** A bot deciding whether to
trust a member (for example, allow-listing which members may issue commands)
must compare the full verifying key.

`message list` and `message stream --format json` both carry
`author_verifying_key`. It is `null` when the author is no longer in the room,
so a `select` on it drops nothing you wanted to keep.

Wherever a command takes `<member-id>` (`ban`, `deputize`, `revoke-deputy`,
`deputies`, `deputized-by`, and `dm send`'s recipient), you can pass either the
8-character short ID or the full verifying key that `member list` prints. The
short ID can be ambiguous; the full key cannot.

Two event fields are not identities you can trust on:

- `reactors` carries short IDs only ([event types](#message-stream---format-json-event-types)).
- `reply_to.author` is a display nickname, not an ID ([`reply_to`](#reply_to)).

## Member management

```bash
riverctl member list         <room-owner-vk>
riverctl member set-nickname <room-owner-vk> "New Nickname"
riverctl member ban          <room-owner-vk> <member-id>
```

`member ban` is not owner-only: the room owner can ban anyone, and so can a
member banning within their own invite subtree, or a deputy of such a member
(see below). `<member-id>` takes a short ID or a full verifying key
([Member IDs and verifying keys](#member-ids-and-verifying-keys)).

### Deputies

A deputy can ban within their deputizer's invite subtree. Deputies are
per-deputizer, not a room-wide flag: "is X a deputy?" is only meaningful as
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
`invite-subtree`), `active`, and `members_deputy_can_ban`.

`members_deputy_can_ban` is how many other members this deputy can ban
within the grant's scope. The contract's own authorization rule decides each
candidate, not a summary of it: `0` for an inactive grant; every other member
for an owner grant; otherwise the members of the deputizer's invite subtree the
contract authorizes this deputy to ban. The deputy is never counted against
themselves: banning yourself is permitted but is not moderation authority. It
is often `0` even for a live grant, because most members have invited nobody.
`members_deputy_can_ban > 0` is the check for "this member actually has moderation
authority here".

The number counts capability, not attribution: a member this deputy could also
ban by some other route still counts, so revoking one grant may not change it.
It also counts direct targets, while a ban cascades to the target's own invite
subtree, so it understates how many members a ban would actually remove.

A grant whose deputizer or deputy has left the room is reported with
`active: false`; the contract does not honour such a grant. `in_room` is `true`
for the room owner even though the owner never appears in the member list.

Nicknames are quoted and escaped in terminal output. They are set by the member
themselves and are printed next to an authority claim, so an unescaped one could
forge a line that reads as a real deputy grant. The `-f json` output carries the
faithful nickname.

`member list --format json` carries `member_id`, `nickname`, `verifying_key`
(the member's full base58 ed25519 key, or `null` when unknown), and the deputy
fields `deputies` (who this member deputized) and `deputized_by` (who deputized
them). `debug room-state` gains `deputy_grant_count` and `deputy_grants`.

Caveats:

- The read commands reject an ambiguous partial member ID and list the matches;
  the write commands (`ban`, `deputize`, `revoke-deputy`) still resolve a
  partial ID to the first match. Pass the full 8-character ID to be certain.
- Like every riverctl command that reads a room, these fetch through the
  standard path, which may PUT migrated state or publish a `member_info` heal
  for your own identity. They never write deputy state.

## Open admission to public rooms

A public room can let newcomers in by proof of work instead of an invitation.

- `riverctl room admission <room>` shows what the room asks of a newcomer
  (an invitation, or proof of work with its difficulty), and with
  `--signing-key` where that key stands in the room.
- `riverctl room join <room> --pow` solves the proof of work and joins as an
  open member, posting a nickname and a first message
  (`--nickname`, `--message`). It makes a new signing key unless you pass
  `--signing-key`; save the key it prints, since nothing else stores it.
  `--extra-bits` (at most 8) solves extra bits beyond the requirement so a
  small rise in difficulty does not invalidate the proof, and `--threads`
  (at most 256) sets the solver threads. Later messages use
  `riverctl message send <room> "text" --signing-key <key>`.
- A room holds at most `--max-provisional` open members (16 at most). When it
  is full a new member is not turned away for being one too many: the open
  members are the authors of the newest open-member messages, so the member
  whose newest message is oldest drops out (entry and messages) and can come
  back by sending a newer message. If a join's own message is older than all
  of theirs, `join` refuses with "the room's open-member slots are held by
  newer messages" and stores nothing; try again later, or ask a member for an
  invitation (`riverctl invite accept`).
- The room owner turns it on with
  `riverctl room config <room> --public-admission true`, and tunes it with
  `--admission-difficulty` (0 to 32 bits), `--admission-new-epoch`
  (invalidates every proof solved so far), `--max-provisional` (1 to 16),
  `--max-provisional-messages`, `--max-provisional-bans` and
  `--admission-listed`. `--public-admission false` turns it off and clears
  the room's open members and bans.

## Command reference

| Group      | Commands                                                                |
|------------|-------------------------------------------------------------------------|
| `room`     | `create`, `list`, `join`, `leave`, `republish`, `config`                |
| `message`  | `send`, `list`, `stream`, `edit`, `delete`, `react`, `unreact`, `reply` |
| `member`   | `list`, `set-nickname`, `ban`, `deputize`, `revoke-deputy`, `deputies`, `deputized-by` |
| `invite`   | `create`, `accept`                                                      |
| `dm`       | `send`, `list`, `invite`, `purge`, `accept`                             |
| `identity` | `whoami`, `export`, `import`                                            |
| `debug`    | Troubleshooting: `contract-get`, `websocket`, `contract-key`, `room-state`, `bans`, `config` |

Run `riverctl <group> --help` or `riverctl <group> <cmd> --help` for full flags. All commands accept `--format json` for scripting.

### `message stream --format json` event types

`message stream` emits one JSON object per line (JSONL). Each carries a `type`:

| `type` | Meaning | Fields |
|--------|---------|--------|
| `message` | A new message | `message_id`, `room`, `author`, `author_verifying_key`, `nickname`, `content`, `timestamp`, `edited`, `reply_to`, `reactions` |
| `edit` | A previously-streamed message's content changed | same as `message` (with the new `content` and `edited: true`) |
| `delete` | A previously-streamed message was deleted | `message_id`, `room`, `author`, `author_verifying_key`, `nickname`, `timestamp` (no `content`) |
| `reaction` | A surfaced message's reactions changed | `message_id`, `room`, `author`, `author_verifying_key`, `nickname`, `timestamp`, `reactions` (emoji → count), `reactors` (emoji → member IDs) |

`author` is the sender's member ID and `author_verifying_key` their full key.
Compare each only with its own kind from `riverctl identity whoami`
([Member IDs and verifying keys](#member-ids-and-verifying-keys)).

On a `reaction` event, `author` and `author_verifying_key` identify the author of
the message that was reacted to, not the person who reacted. Read `reactors`
for that.

**`reactors` carries short IDs only.** A bot that acts on who reacted (a
moderation sweep, an "admin reacted ✅ to approve" flow) is deciding on 40 bits.
It must resolve those IDs through `member list`'s `verifying_key` before
treating one as an identity. Do not substitute the event's
`author_verifying_key`: on a reaction event it names an uninvolved party.

`edit`/`delete` events are emitted only for messages the stream actually surfaced. Bridges should key off `type` and tolerate unknown future types **and unknown fields**. New fields are added to these events (this table has grown twice), so a strict schema that rejects them will break on an ordinary riverctl upgrade.

#### `reply_to`

`reply_to` is `{ "author", "author_id", "message_id", "preview" }` only when
riverctl could read the quoted message back from live room state. Inside it,
`author` is a display **nickname**, not a member ID. The ID there is
`author_id`, and it has no full-key sibling, so it is not something to trust on.

`reply_to` is `null` for a message that is not a reply, and also for a reply
whose quoted message riverctl could not verify because:

- its author was banned (which purges their messages);
- it was deleted;
- it aged out of the room's `max_recent_messages` window;
- it could not be decrypted.

The replier writes the quoted author and preview into the reply, and nothing
validates them. riverctl never emits them unverified; otherwise a bridge would
republish a banned member's text after the ban.

The human-readable output marks a reply it could not verify with a
`[reply to an unavailable message]` prefix. The JSON deliberately does not
distinguish it from a non-reply, so no existing consumer breaks on a new shape.

The one exception is a private room `riverctl` holds no secrets for at all.
There the human-readable output adds no marker either: riverctl cannot read any
of the room's messages, and every body already renders `<encrypted>`.

### Running `message stream` as a long-lived bot

River occasionally re-keys its room contract, which moves every room to a new
address. A running `message stream` checks for this every few minutes. When it
sees one, it **exits with status 75** and says so on stderr:

```text
River's pointer now names a different room-contract generation than this command started with.
  was: <old generation>
  now: <new generation>
Exiting with status 75 so it can be restarted; a restarted riverctl uses the current generation.
```

A restarted `riverctl` looks up the new address and carries on streaming from
it. **Run a bot under something that restarts it on any failure, with a short
delay, not only on status 75.** Right after a re-key the restarted process can
fail until the room has moved to the new address, and a wrapper that gives up
on anything other than 75 would stop the bot for good. A bot with no restart
wrapper stops after a re-key.

With systemd, the relevant `[Service]` lines:

```ini
[Service]
ExecStart=/usr/local/bin/riverctl message stream <room-owner-vk> --format json
Restart=on-failure
RestartSec=5
```

Without `RestartSec`, a few quick failures in a row can trip systemd's start
limit and leave the unit stopped.

Or as a script (save it to a file rather than pasting it into an interactive
shell):

```bash
#!/bin/sh
while true; do
  riverctl message stream <room-owner-vk> --format json
  status=$?
  [ "$status" -eq 75 ] || echo "riverctl exited with status $status; restarting" >&2
  sleep 5
done
```

Status 75 is `EX_TEMPFAIL` ("temporary failure, retry"). Every other failure
exits with status 1. The distinct status lets a wrapper tell an expected restart
from a real failure, for example to alert only on the latter. Other failures
still need a restart.

**Restarting can re-deliver messages.** A restarted stream has no memory of what
it already emitted. With `--initial-messages N`, it re-emits the last N messages
as new: that covers up to N messages sent while it was restarting, and repeats
ones you had already seen. If the bot acts on messages, deduplicate on
`message_id`. Without `--initial-messages`, nothing is repeated, but messages
sent during the restart are not shown.

If River has re-keyed to a version newer than your riverctl, the restarted
process still streams the room once it has been moved to the new address, and
prints a warning telling you to upgrade (`cargo install riverctl`). An
up-to-date River client moves the room when someone opens it. Until then,
`--subscribe` keeps failing and being restarted, while polling mode stays
running and keeps retrying.

## Configuration

- `--node-url <URL>`: override the Freenet node URL (default `ws://127.0.0.1:7509/...`).
- `--config-dir <PATH>`: override where `riverctl` stores room data and signing keys (then the `RIVER_CONFIG_DIR` env var; default is the platform data dir, e.g. `~/.local/share/river` on Linux).
- `--log-file <PATH>`: write logs to a file instead of stderr (stdout is reserved for command output).
- `RIVERCTL_LOG_FILE` env var: same as `--log-file`.

## Links

- [River repository](https://github.com/freenet/river)
- [Freenet website](https://freenet.org)
- [Freenet quickstart](https://freenet.org/quickstart/)

## For contributors

A change to `common/`, `contracts/` or `delegates/` can change the WASM, and
with it the contract or delegate key. Follow the migration workflow in
`.claude/rules/delegate-migration.md` (at the repository root) before
rebuilding. Two tasks keep the CLI's bundled copy in sync:

| Task | What it does |
|---|---|
| `cargo make sync-wasm` | Rebuilds every contract and delegate WASM and copies each to its committed locations, `cli/contracts/` included |
| `cargo make sync-cli-wasm` | Copies `ui/public/contracts/room_contract.wasm` to `cli/contracts/` |

The CLI build fails if the bundled WASM drifts from the most recently built
artifact.

An integration smoke test lives at `tests/message_flow.rs`. It is `#[ignore]` by
default; run it manually with:

```bash
cargo test --test message_flow -- --ignored --nocapture
```

The harness builds the Freenet binary from a freenet-core checkout. It reads
`FREENET_CORE_PATH`, and otherwise assumes `../../../freenet-core/main` relative
to `cli/`, which is `~/code/freenet/freenet-core/main` when River is checked out
at `~/code/freenet/river/main`.
