# River - Decentralized Chat on Freenet

River is a decentralized group chat system built on Freenet, designed to provide a secure and
upgradeable alternative to traditional chat platforms. It features a web-based interface built with
[Dioxus](https://dioxuslabs.com) and a modular contract architecture using the freenet-scaffold
framework.

![Screenshot of chat interface](screenshot-20241009.png)

## Roadmap (Jan 2025)

- [x] [Scaffold library](https://github.com/freenet/river/tree/main/scaffold) and
      [macro](https://github.com/freenet/river/tree/main/scaffold-macro) to simplify contract
      development
  - [ ] Move scaffold and scaffold-macro to separate crates
- [x] Basic
      [chat room contract](https://github.com/freenet/river/blob/main/common/src/room_state.rs)
  - [x] Invite-only rooms
  - [ ] Private rooms
  - [ ] One-click invite links and other access-control mechanisms
  - [ ] [GhostKey](https://freenet.org/ghostkey) support as alternative to invite-only rooms
- [x] Web-based [user interface](https://github.com/freenet/river/tree/main/ui) implemented in
      Dioxus allowing viewing and modifying the chat room state
- [ ] Integration with Freenet to synchronize room contracts over the network _(currently working on
      this)_
- [ ] Quantum-safe cryptography
- [ ] Message search and filtering

## Getting Started

### Building and Running the UI

To build and run the River UI locally for testing:

1. Install dependencies:

   ```bash
   # Install Rust using rustup
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

   # Add the wasm target
   rustup target add wasm32-unknown-unknown

   # Install ssl development library
   # This example is for Ubuntu and may be different on your system
   sudo apt-get install libssl-dev

   # Install build tools
   cargo install dioxus-cli
   cargo install cargo-make
   ```

2. Build and run with example data:

   ```bash
   # Clone the repository
   git clone git@github.com:freenet/river.git

   # Enter the repository
   cd river

   # Initialize freenet submodule
   git submodule init
   git submodule update

   # Run development server with example data
   cargo make dev-example
   ```

3. Open http://localhost:8080 in your browser

The UI will run with example data and without attempting to sync with Freenet, making it ideal for
testing and development.

### Key Development Features

- **example-data**: Populates the UI with sample rooms and messages
- **no-sync**: Disables Freenet synchronization for local testing

These features can be combined when building:

```bash
# Build with example data
cargo make build-ui-example

# Build without Freenet sync
cargo make build-ui-no-sync

# Build with both features
cargo make build-ui-example-no-sync
```

### Joining a Real Room

To join a River chat room on Freenet, you'll need:

1. The room's contract address (derived from its public key)
2. An invitation from an existing member

River runs in your browser, and is built to work both on mobile phones and desktop computers.

1. Install Freenet
2. Click a link to launch River in your browser
3. Create or join a room using its contract address
   - To join an existing room you need an invitation from a current member
4. Choose your nickname and start chatting

The interface provides tools for:

- **Member Management**: Invite, manage, and moderate members through an intuitive UI
- **Room Settings**: Configure room parameters and permissions

## Technical Details

### Project Structure

- [common](common/): Shared code for contracts and UI
- [ui](ui/): Web-based user interface
- [contracts](contracts/): River chat room contract implementation

### Access Control

River uses an **invitation tree** model for managing room membership:

- The room owner sits at the root of the tree
- Each member can invite others, creating branches
- Members can ban users they invited or anyone downstream
- Future versions will support alternative mechanisms like:
  - [GhostKeys](https://freenet.org/ghostkey) for anonymous participation
  - One-click invite links for easier onboarding

### Privacy Model

- **Public Rooms**: Readable by anyone with the contract address
- **Private Rooms** (Future): End-to-end encrypted using symmetric keys
- **Quantum Resistance** (Future): Upgradeable to post-quantum crypto

### Architecture

The system is built using:

- **freenet-scaffold**: A Rust macro/crate for composable contract development
- **Elliptic Curve Cryptography**: For authentication and message signing
- **CBOR Serialization**: Efficient binary format for state storage
- **Dioxus**: Rust framework for building reactive web UIs

## Membership Management

River uses a flexible system for controlling room membership, starting with invitations but designed
to support multiple mechanisms. This helps prevent spam while allowing room owners to maintain
healthy communities.

### Current Mechanism: Invitation Tree

The initial implementation uses an invitation tree where:

- Each room has an owner who forms the root
- Members can invite others, creating branches
- Members can manage users they invited or anyone downstream
- This creates a hierarchical structure for managing permissions

### Future Mechanisms

We're developing additional membership options:

- **GhostKeys**: Anonymous participation using temporary identities
- **One-click Links**: Easy onboarding without manual invitations
- **Public Rooms**: Open participation with moderation tools
- **Private Rooms**: End-to-end encrypted with invite-only access

```
Room: freenet (Owner: owner)
│
├─── User: alice
│    │
│    ├─── User: bob
│    │    │
│    │    └─── User: charlie
│    │
│    ├─── User: dave
│    │
│    └─── User: eve
│
└─── User: frank
```

## Permissioning Example

Consider the scenario where "alice" invites "bob", who subsequently invites "charlie". If "alice"
decides to ban "charlie" from the room, she can directly enforce this action, exercising authority
over users invited by her or those invited further down the chain.

```
Room: freenet (Owner: owner)
│
├─── User: alice
│    │
│    ├─── User: bob
│    │    │
│    │    └─── Banned User: charlie
│    │
│    ├─── User: dave
│    │
│    └─── User: eve
│
└─── User: frank
```

In this example:

- "alice", being higher in the invite chain, has the authority to ban "charlie" directly,
  irrespective of "bob" inviting "charlie" to the room.
- This illustrates how permissioning cascades down the invitation tree, enabling users higher in the
  hierarchy to enforce rules and manage the behavior of users beneath them.

## Web Interface

River provides a modern web-based interface built with [Dioxus](https://dioxuslabs.com), making it
accessible from any device with a web browser.

### Best Practices

1. **Intuitive UI**: The web interface provides clear visual feedback and guidance for all actions
2. **Error Handling**: The UI gracefully handles common scenarios like:
   - Attempting to join a room without an invitation
   - Managing duplicate nicknames
   - Handling invalid room addresses
   - Preventing duplicate invitations
3. **Accessibility**: The interface follows web accessibility standards for inclusive use
4. **Responsive Design**: Works seamlessly across desktop and mobile devices
5. **Progressive Enhancement**: Core functionality works even with limited browser features

## Contract Architecture

The chat room contract is implemented using Freenet's composable state pattern. The core state
structure is defined in [common/src/room_state.rs](common/src/room_state.rs):

```rust
pub struct ChatRoomStateV1 {
    pub configuration: AuthorizedConfigurationV1, // Room settings and limits
    pub bans: BansV1,                             // List of banned users
    pub members: MembersV1,                       // Current room members
    pub member_info: MemberInfoV1,                // Member metadata like nicknames
    pub recent_messages: MessagesV1,              // Recent chat messages
    pub upgrade: OptionalUpgradeV1,               // Optional upgrade to new contract
}
```

Each component is implemented as a separate module with its own state management:

- [Configuration](common/src/room_state/configuration.rs): Room settings and limits
- [Bans](common/src/room_state/ban.rs): User banning and moderation
- [Members](common/src/room_state/member.rs): Room membership and invitations
- [Member Info](common/src/room_state/member_info.rs): Member metadata and nicknames
- [Messages](common/src/room_state/message.rs): Chat message handling
- [Upgrades](common/src/room_state/upgrade.rs): Contract upgrade mechanism

The contract uses CBOR serialization via [ciborium](https://crates.io/crates/ciborium) for efficient
storage and transmission. All state changes are signed using elliptic curve cryptography to ensure
authenticity.

## License

River is open-source software licensed under the MIT License. See [LICENSE](LICENSE) for details.
