# River for the desktop (preview)

This is a native desktop app for [River](../../README.md), the decentralized group chat that runs on
[Freenet](https://freenet.org). It is built on the Rune GPU UI toolkit and is separate from the web
UI: it talks to your Freenet node directly, and it uses the same rooms, invite codes and identity
export format as the web UI.

It is an early version that you build from source today. It already covers rooms, messages, replies,
reactions, direct messages, invitations, search, notifications with Do not disturb, running in the
tray, and settings. **Please build it, try it, and tell us what breaks.** The checklist and the
reporting steps are below.

## Requirements

You need three things on every system:

- **Rust**, installed with [rustup](https://rustup.rs/). The repository pins its own compiler in
  `apps/rust-toolchain.toml`, and rustup installs it the first time you build.
- **A Freenet node** on the same computer, or one you can reach (see
  [Connecting to a node elsewhere](#connecting-to-a-node-elsewhere)). If River finds no node on this
  computer, its first screen offers to install Freenet for you. You can also install it yourself
  from the [Freenet quickstart](https://freenet.org/quickstart/).
- **A GPU.** River draws with the GPU through wgpu. On Linux that means a driver with Vulkan
  support. Software rendering is only documented for headless screenshots (see Hacking on it).

**Linux** also needs these system libraries. On Debian and Ubuntu:

```bash
sudo apt-get install libxkbcommon-dev libwayland-dev libfontconfig1
```

`libxkbcommon-dev` and `libwayland-dev` are needed to build, which is what CI installs.
`libfontconfig1` is only needed at run time: River loads it to find your system fonts, and starts
without it but with fewer fonts. On Fedora and Arch the font package is `fontconfig`.

CI builds and tests macOS and Windows with a plain Rust setup and installs no extra system packages
there.

## Build and run

```bash
git clone https://github.com/freenet/river
cd river/apps
cargo run -p river-desktop --release
```

The first build downloads and compiles Rune and its dependencies and takes a few minutes. The
binary is `apps/target/release/river` (`river.exe` on Windows). Check it with:

```bash
./target/release/river --version     # prints: river 0.1.0
./target/release/river --node-check  # prints the node it would use, then exits
```

For a faster build while you experiment, leave out `--release` (`cargo run -p river-desktop`). The
binary is then `apps/target/debug/river`, and it runs slower.

`apps` is its own Cargo workspace with its own `Cargo.lock`, so run these commands from the `apps`
directory, not from the top of the repository.

On Windows, the release build has no console. `river.exe` returns your prompt immediately and
prints nothing; use `--node-check` from a debug build if you need console output.

### Connecting to a node elsewhere

River uses the Freenet node on this computer, at `127.0.0.1:7509`. To use another one, pass
`--node host:port` or set `FREENET_NODE`:

```bash
./target/release/river --node 192.168.1.20:7509
FREENET_NODE=localhost:7600 ./target/release/river
```

The order is: `--node`, then `FREENET_NODE`, then the address saved from the app's own screens
(Settings > Connection), then the default. When you name a node, River never installs, starts or
stops Freenet on this computer.

A node on another computer is reached without encryption and without a login. Use an SSH tunnel or
a VPN, and point River at the local end of the tunnel.

## First run

1. **Freenet.** If a node is running, River connects to it. If not, it offers to install Freenet
   and start it.
2. **Permission.** The first time, River asks the node for permission to open your rooms. Press the
   button in the app. It opens a page in your browser on your node's own address, not on River.
   Choose Allow there. Your node must be Freenet 0.2.156 or newer.
3. **Identity and rooms.** River keeps your rooms and identity in your Freenet node. To bring an
   identity over from the web UI, use **Export ID** there, then **Settings > Identity > Import identity** in the
   app (or **Import it** in the Join dialog), and paste the token.
4. **Join a room.** Choose **Enter invite code**, paste an invite code or link, and press Join room.
   You can also create a room with the plus button.

River saves a few small files in your per-user config directory (on Linux, `~/.config/river`, or `river` under `$XDG_CONFIG_HOME`): the
node address you chose, the permission your node granted, and the background settings. Rooms,
messages and identity are not stored there.

**Settings** is the gear in the room list header, or Ctrl+, (Cmd+, on a Mac). Press Ctrl+/ (Cmd+/)
to see every keyboard shortcut.

### Closing the window and quitting

When the system has a tray, closing the window hides it and River keeps running, so notifications
keep arriving. This is on by default and can be turned off in **Settings > Background and startup**,
where you can also have River start when you log in (it then starts hidden). To quit for real, use
**Quit River** in that section, **Quit** in the tray menu, or Ctrl+Q. Without a tray, closing the
window quits. The first time closing hides River, it shows a notification saying so and how to
quit; it is not shown again.

On Windows the tray icon may be in the hidden icons area (the `^` by the clock), where new icons
start.

Only one River runs per user. Starting it a second time shows the window of the first one.

## What to try

Everything here exists on `main`. Chords use Cmd instead of Ctrl on a Mac.

- Send a message, reply to one, react with an emoji, and edit your last message (Up in an empty box).
  Click a reply's quoted strip to jump to the message it quotes.
- Type `@` to mention a member and `:smile` for an emoji.
- Open a member's dialog and press **Message** to send a direct message. Try archiving a thread.
- Jump to a room or member with Ctrl+K. Search the open room with Ctrl+F.
- Turn on Do not disturb (the row in the room list, or Settings > Notifications) and check that
  notifications stop and return.
- Close the window with the tray running, reopen it from the tray, and quit with Ctrl+Q.
- Make the window narrow (down to 640x480) and zoom with Ctrl+= and Ctrl+- (Ctrl+0 resets).
- Export your identity from the app, import it in the web UI, and the other way round.
- Open Settings > Connection and **Node status**, and check that they describe your node correctly.

## Reporting problems

Open an issue at <https://github.com/freenet/river/issues> and include:

- the output of `river --version`, and your operating system and version,
- what you did, and what you expected to happen,
- the node status details: open **Node status** and press **Copy diagnostics (includes node
  address)**, then paste the result. It lists versions, your OS, the node's host and port and the
  connection state, so read it before posting if the address is private to you,
- the log file (see [The log file](#the-log-file)), with the time you noticed the problem,
- a screenshot, if the problem is visual.

### The log file

River reports what goes wrong with itself (lost connections, rooms it could not read, updates
it refused) on its standard error, with the time (UTC) at the start of each line. A Windows
release build has no console, and a River started from a launcher or at login can have no
standard error at all, so there the lines go nowhere. When River finds that nothing is reading
its standard error (it is closed, or goes to the null device), it writes the lines to a log file
instead. If River was started from a terminal, with its output sent to a file (`river 2>
river-log.txt`), or into a pipe or the system's log (journald), it leaves the output there and
keeps no log file. (On Linux, a River started from a desktop launcher or at login usually has
its output in the user journal: try `journalctl --user -b` and look for `river`.)
**Settings > Diagnostics** says which, and when there is a file it shows the
path and has an **Open log folder** button. The file is `river.log` in the `logs` folder of
River's config folder:

- Linux: `~/.config/River/logs/river.log` (or under `$XDG_CONFIG_HOME`)
- Windows: `%LOCALAPPDATA%\Freenet\River\config\logs\river.log`
- macOS: under `~/Library/Application Support` (Settings shows the exact folder)

River starts a new file only when it starts: if `river.log` is over 2 MB then, it is renamed to
`river.log.1` (replacing the one before). A River that runs for a long time and reports a lot
can make the file larger than that until its next start. Attach `river.log`, and `river.log.1`
if the problem is older than the newest file.

River does not write message text, nicknames or signing keys to it, but it does write room keys
and contract keys, and your user name where it is part of a path. Look through the file before
you share it, and tell us if you find a line that should not be there.

To capture one run: from a terminal, `river 2> river-log.txt` (on Windows, `river.exe 2>
river-log.txt` from a command prompt; the release build then writes to that file). Or quit River,
move the two log files away, start River, trigger the problem, and attach the new `river.log`.

**Never paste an exported identity token, an invite code or a private key.** An exported identity
token contains your private key, and anyone who has it can write as you in your rooms.

## Hacking on it

- `apps/river-app` is the UI and the state it keeps, shared by every platform.
- `apps/desktop` is the operating-system shell: the window, tray, notifications, start at login,
  installing Freenet, and updates.

Run the same checks CI runs, from `apps`:

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                    # or: cargo test -p river-app -p river-desktop
```

Debug builds can render a screen headless, with no node and no window:

```bash
cargo build -p river-desktop
./target/debug/river --list-scenes
./target/debug/river --snap out.png --scene chat
./target/debug/river --snap narrow.png --scene chat-narrow --size 820x720
```

Release builds do not include `--snap` or `--list-scenes`. If rendering fails because there is no
GPU, install `mesa-vulkan-drivers` and set `VK_ICD_FILENAMES` to its `lvp_icd.json`, as CI does.

`apps/scripts/fresh-node-e2e.sh <work dir>` runs the app against two fresh local Freenet nodes. It
needs a `freenet` binary (set `FREENET_BIN`), `openssl` and `curl`.

The design rules, what the app writes to the node, and how Rune is updated are in
[`.claude/rules/native-apps.md`](../../.claude/rules/native-apps.md).

## Known limits

- There is no published release yet, so there are no installers and no signed builds. Building from
  source is the way to try it today ([#881](https://github.com/freenet/river/issues/881) tracks the
  release). The app has an update mechanism for signed releases, but it has nothing to offer until
  one exists.
- Linux is the best-tested platform. The macOS and Windows builds are compiled and tested by CI but
  have had much less use.
- On Windows, a River started from PowerShell does not stop with Ctrl+C
  ([#966](https://github.com/freenet/river/issues/966)).
- There is no light theme yet.
