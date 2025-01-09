# River - Decentralized Chat on Freenet

![Screenshot of chat interface](screenshot-20241009.png)

River is a decentralized group chat system built on Freenet, designed to provide a secure and
upgradeable alternative to traditional chat platforms. It features a web-based interface built
with [Dioxus](https://dioxuslabs.com) and a modular contract architecture using the freenet-scaffold
framework.

## Project Structure

- [common](common/): Shared code for contracts and UI
- [ui](ui/): Web-based user interface
- [contracts](contracts/): River chat room contract implementation

## Key Features

🌐 **Web-based Interface** - Modern web UI built with Dioxus for cross-platform compatibility  
🔒 **Secure by Design** - Uses elliptic curve cryptography for authentication and signing  
🔄 **Upgradeable** - Flexible upgrade mechanism for both UI and contracts  
🌱 **Extensible** - Open architecture allows alternative UIs and integrations  
📜 **Modular Contracts** - Built using freenet-scaffold for composable state management  
📦 **Efficient Storage** - Uses CBOR serialization via [ciborium](https://crates.io/crates/ciborium)  

## Getting Started

To join a River chat room, you'll need:
1. The room's contract address (derived from its public key)
2. An invitation from an existing member

Once invited, you can:
- Choose your own nickname (changeable at any time)
- Participate in chat conversations
- Invite others to join

## Technical Details

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

## Contributing

We welcome contributions! River is designed to be extensible:
- Build alternative UIs using the River contract
- Create integrations with other systems
- Develop new features and improvements

Check out our [contribution guidelines](CONTRIBUTING.md) to get started.

## Roadmap

- [ ] Private room encryption
- [ ] GhostKeys support
- [ ] One-click invite links
- [ ] Quantum-resistant crypto integration
- [ ] Mobile-friendly UI
- [ ] Message search and filtering

## License

River is open-source software licensed under the MIT License. See [LICENSE](LICENSE) for details.

Absolutely, let's refine it for a more concise and technical approach, akin to an RFC (Request for
Comments):

# Membership Management

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

# Web Interface

River provides a modern web-based interface built with [Dioxus](https://dioxuslabs.com), making it
accessible from any device with a web browser.

## Key Features

- **Room Creation**: Easily create new chat rooms with custom settings
- **Member Management**: Invite, manage, and moderate members through an intuitive UI
- **Real-time Chat**: Smooth messaging experience with message history
- **Settings Management**: Configure room parameters and permissions
- **Cross-platform**: Works on desktop and mobile browsers

## Getting Started

1. Open the River web interface
2. Create or join a room using its contract address
3. If joining an existing room, request an invitation from a current member
4. Once invited, choose your nickname and start chatting

The interface provides visual tools for:
- Managing room membership
- Viewing message history
- Handling invitations and bans
- Configuring room settings

# Design

## Contract Parameters

```rust
#[derive(Serialize, Deserialize)]
pub struct ChatParameters {
    pub owner: ECPublicKey,
}
```

## Contract State

Represents the state of a chat room contract, including its configuration, members, recent messages,
recent user bans, and information about a replacement chat room contract if this one is out-of-date.

```rust
#[derive(Serialize, Deserialize)]
pub struct ChatState {
    pub configuration: AuthorizedConfiguration,

    /// Any members excluding any banned members (and their invitees)
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,

    /// Any authorized messages (which should exclude any messages from banned users)
    pub recent_messages: Vec<AuthorizedMessage>,
    pub ban_log: Vec<AuthorizedUserBan>,
}

#[derive(Serialize, Deserialize)]
pub struct AuthorizedConfiguration {
    pub configuration: Configuration,
    pub signature: ECSignature,
}

#[derive(Serialize, Deserialize)]
pub struct Configuration {
    pub configuration_version: u32,
    pub name: String,
    pub max_recent_messages: u32,
    pub max_user_bans: u32,
}

#[derive(Serialize, Deserialize)]
pub struct AuthorizedMember {
    pub member: Member,
    pub invited_by: ECPublicKey,
    pub signature: ECSignature,
}

#[derive(Serialize, Deserialize)]
pub struct Member {
    pub public_key: ECPublicKey,
    pub nickname: String,
}

#[derive(Serialize, Deserialize)]
pub struct AuthorizedUpgrade {
    pub upgrade: Upgrade,
    pub signature: ECSignature,
}

#[derive(Serialize, Deserialize)]
pub struct Upgrade {
    pub version: u8,
    pub new_chatroom_address: Blake3Hash,
}

#[derive(Serialize, Deserialize)]
pub struct AuthorizedMessage {
    pub time: SystemTime,
    pub content: String,
    pub signature: ECSignature, // time and content
}

#[derive(Serialize, Deserialize)]
pub struct AuthorizedUserBan {
    pub ban: UserBan,
    pub banned_by: ECPublicKey,
    pub signature: ECSignature,
}

#[derive(Serialize, Deserialize)]
pub struct UserBan {
    pub banned_at: SystemTime,
    pub banned_user: ECPublicKey,
}
```

## Contract Summary

Summarizes the state of a chat room contract, must be compact but must contain enough information
about the contract to create a delta that contains whatever is in the state that isn't in the
summarized state.

```rust
#[derive(Serialize, Deserialize)]
pub struct ChatSummary {
    pub configuration_version: u32,
    pub member_hashes: HashSet<u64>,
    pub upgrade_version: Option<u8>,
    pub recent_message_hashes: HashSet<u64>,
    pub ban_log_hashes: Vec<u64>,
}
```

## Contract Delta

Efficiently represents the difference between two chat room contract states, including
configuration, members, recent messages, recent user bans, and a replacement chat room contract. It
can be assumed that once replaced, no further changes will be made to a chat room (which means it
will be a footgun, so extreme care will be necessary when upgrading a contract).

```rust
#[derive(Serialize, Deserialize)]
pub struct ChatDelta {
    pub configuration: Option<AuthorizedConfiguration>,
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,
    pub recent_messages: Vec<AuthorizedMessage>,
    pub ban_log: Vec<AuthorizedUserBan>,
}
```

# Local Data Storage

Use a library like [confy](https://crates.io/crates/confy) for storing local configuration in
whatever way is standard for the OS. The CLI should store:

- Users, nickname, public key, and private key
- Rooms - name and Freenet identifier

# Best Practices

1. **Intuitive UI**: The web interface provides clear visual feedback and guidance for all actions
2. **Error Handling**: The UI gracefully handles common scenarios like:
   - Attempting to join a room without an invitation
   - Managing duplicate nicknames
   - Handling invalid room addresses
   - Preventing duplicate invitations
3. **Accessibility**: The interface follows web accessibility standards for inclusive use
4. **Responsive Design**: Works seamlessly across desktop and mobile devices
5. **Progressive Enhancement**: Core functionality works even with limited browser features
