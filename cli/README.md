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

Each room uses a separate signing key. Export it to move between machines or back it up:

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
| `identity` | `export`, `import`                                                      |
| `debug`    | troubleshooting utilities                                               |

Run `riverctl <group> --help` or `riverctl <group> <cmd> --help` for full flags. All commands accept `--format json` for scripting.

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
