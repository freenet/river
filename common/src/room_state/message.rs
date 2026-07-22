use crate::room_state::member::MemberId;
use crate::room_state::privacy::{PrivacyMode, SecretVersion};
use crate::room_state::ChatRoomParametersV1;
use crate::util::sign_struct;
use crate::util::{truncated_base64, verify_struct};
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::time::SystemTime;

/// Ciphertext overhead added by AES-256-GCM (`encrypt_with_symmetric_key`):
/// the 16-byte authentication tag appended to the plaintext. The nonce lives
/// in a separate field of [`RoomMessageBody::Private`] and does not count
/// toward [`RoomMessageBody::content_len`]. Pinned against real encryption
/// by the `measure_*_matches_private_*` tests (feature `ecies-randomized`).
pub const ENCRYPTION_TAG_OVERHEAD: usize = 16;

/// Computed state for message actions (edits, deletes, reactions)
/// This is rebuilt from action messages and not serialized
#[derive(Clone, PartialEq, Debug, Default)]
pub struct MessageActionsState {
    /// Messages that have been edited: message_id -> new text content
    pub edited_content: HashMap<MessageId, String>,
    /// Messages that have been deleted
    pub deleted: std::collections::HashSet<MessageId>,
    /// Reactions on messages: message_id -> (emoji -> list of reactors)
    pub reactions: HashMap<MessageId, HashMap<String, Vec<MemberId>>>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct MessagesV1 {
    pub messages: Vec<AuthorizedMessageV1>,
    /// Computed state from action messages (not serialized - rebuilt on each delta)
    #[serde(skip)]
    pub actions_state: MessageActionsState,
}

impl ComposableState for MessagesV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = Vec<MessageId>;
    type Delta = Vec<AuthorizedMessageV1>;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let members_by_id = parent_state.members.members_by_member_id();
        let owner_id = parameters.owner_id();

        for message in &self.messages {
            let verifying_key = if message.message.author == owner_id {
                // Owner's messages are validated against the owner's key
                &parameters.owner
            } else if let Some(member) = members_by_id.get(&message.message.author) {
                // Regular member messages are validated against their member key
                &member.member.member_vk
            } else {
                return Err(format!(
                    "Message author not found: {:?}",
                    message.message.author
                ));
            };

            if message.validate(verifying_key).is_err() {
                return Err(format!("Invalid message signature: id:{:?}", message.id()));
            }
        }

        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.messages.iter().map(|m| m.id()).collect()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let delta: Vec<AuthorizedMessageV1> = self
            .messages
            .iter()
            .filter(|m| !old_state_summary.contains(&m.id()))
            .cloned()
            .collect();
        if delta.is_empty() {
            None
        } else {
            Some(delta)
        }
    }

    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        let max_recent_messages = parent_state.configuration.configuration.max_recent_messages;
        let max_message_size = parent_state.configuration.configuration.max_message_size;
        let privacy_mode = &parent_state.configuration.configuration.privacy_mode;

        // Validate message constraints before adding
        if let Some(delta) = delta {
            for msg in delta {
                let content = &msg.message.content;

                match content {
                    RoomMessageBody::Private { secret_version, .. } => {
                        // In private mode, accept any secret_version that has a
                        // corresponding signed record in `parent_state.secrets.versions`.
                        //
                        // Previously this required `secret_version == current_version`
                        // AND `has_complete_distribution` to be true for every current
                        // member. That was too strict in two ways:
                        //
                        // 1. **Strict-version mismatch (Bug #3, Ivvor's repro):** if the
                        //    owner has rotated to v_new (e.g. after a ban or membership
                        //    churn) and sends a message at v_new, but the invitee's
                        //    secrets-state hasn't caught up yet (still at v_old, or has
                        //    v_old + v_new but `current_version` is still v_old), the
                        //    composable `apply_delta` short-circuited via `?` and dropped
                        //    the entire delta — including the message itself,
                        //    membership updates, and any secrets-delta in the same
                        //    payload. The invitee's UI would never even see the
                        //    encrypted message; back-fill became impossible.
                        //
                        // 2. **Complete-distribution freeze:** a single member missing a
                        //    blob at `current_version` froze the entire room for
                        //    messages, with no recovery path unless that member came
                        //    online and the owner re-issued blobs.
                        //
                        // Author safety is already enforced by `MessagesV1::verify`'s
                        // member-or-owner signature check (see lines 47-66 above) and by
                        // `ChatRoomStateV1::post_apply_cleanup`'s ban sweep. The
                        // secret_version → version-record cross-check below ensures
                        // the message references a real, owner-signed version, so a
                        // malicious peer can't inject ciphertext at a fabricated
                        // version number.
                        //
                        // **Trade-off acknowledged (Codex review, 2026-05-17):** this
                        // relaxation permits a member with a stale client to send a
                        // message encrypted at an older `secret_version` AFTER the
                        // room has rotated. Members previously holding that older
                        // secret (e.g. banned members) could still decrypt such a
                        // message. We accept this because:
                        //   - banned members already hold the plaintext of ALL
                        //     messages sent during the old version's tenure, so the
                        //     marginal post-rotation exposure is small and bounded
                        //     by how quickly senders catch up to the latest version;
                        //   - the alternative (`secret_version == current_version`,
                        //     i.e. the pre-fix rule) is what produced Bug #3 in the
                        //     first place — receivers whose own state lagged the
                        //     sender's `current_version` dropped every message they
                        //     received, including legitimate ones from non-stale
                        //     senders;
                        //   - confidentiality of post-rotation messages is properly
                        //     enforced at the SENDER, not the contract: senders
                        //     should always encrypt with the latest secret they
                        //     have. PR B will add the UI back-fill needed for
                        //     stragglers to rotate forward.
                        if *privacy_mode == PrivacyMode::Private
                            && !parent_state
                                .secrets
                                .versions
                                .iter()
                                .any(|v| v.record.version == *secret_version)
                        {
                            return Err(format!(
                                "Private message references unknown secret version {}",
                                secret_version
                            ));
                        }
                    }
                    RoomMessageBody::Public { .. } => {
                        // In private mode, reject public messages (everything must be encrypted)
                        // Exception: event messages (joins, etc.) contain no sensitive content
                        if *privacy_mode == PrivacyMode::Private && !content.is_event() {
                            return Err("Cannot send public messages in private room".to_string());
                        }
                    }
                }
            }

            // Deduplicate by message ID to prevent duplicate messages from race conditions
            let existing_ids: std::collections::HashSet<_> =
                self.messages.iter().map(|m| m.id()).collect();
            self.messages.extend(
                delta
                    .iter()
                    .filter(|msg| !existing_ids.contains(&msg.id()))
                    .cloned(),
            );
        }

        // Always enforce message constraints
        // Ensure there are no messages over the size limit
        self.messages
            .retain(|m| m.message.content.content_len() <= max_message_size);

        // Ensure all messages are signed by a valid member or the room owner, remove if not
        let members_by_id = parent_state.members.members_by_member_id();
        let owner_id = MemberId::from(&parameters.owner);
        self.messages.retain(|m| {
            members_by_id.contains_key(&m.message.author) || m.message.author == owner_id
        });

        // Sort messages by time, with MessageId as secondary sort for deterministic ordering
        // (CRDT convergence requirement - without this, ties produce non-deterministic order)
        self.messages.sort_by(|a, b| {
            a.message
                .time
                .cmp(&b.message.time)
                .then_with(|| a.id().cmp(&b.id()))
        });

        // Remove oldest messages if there are too many
        if self.messages.len() > max_recent_messages {
            self.messages
                .drain(0..self.messages.len() - max_recent_messages);
        }

        // Rebuild computed state from action messages
        self.rebuild_actions_state();

        Ok(())
    }
}

impl MessagesV1 {
    /// Rebuild the computed actions state by scanning all action messages.
    ///
    /// This method only processes PUBLIC action messages. For private rooms,
    /// use `rebuild_actions_state_with_decrypted` and provide the decrypted
    /// content for each private action message.
    pub fn rebuild_actions_state(&mut self) {
        self.rebuild_actions_state_with_decrypted(&HashMap::new());
    }

    /// Rebuild actions state with decrypted content for private action messages.
    ///
    /// For private rooms, the caller should decrypt each private action message
    /// and provide the plaintext bytes in `decrypted_content`, keyed by message ID.
    ///
    /// # Arguments
    /// * `decrypted_content` - Map of message_id -> decrypted plaintext bytes for
    ///   private action messages. Public actions are decoded directly.
    pub fn rebuild_actions_state_with_decrypted(
        &mut self,
        decrypted_content: &HashMap<MessageId, Vec<u8>>,
    ) {
        use crate::room_state::content::{
            ActionContentV1, DecodedContent, ACTION_TYPE_DELETE, ACTION_TYPE_EDIT,
            ACTION_TYPE_REACTION, ACTION_TYPE_REMOVE_REACTION,
        };

        // Clear existing computed state
        self.actions_state = MessageActionsState::default();

        // Build a map of message_id -> author for authorization checks
        let message_authors: HashMap<MessageId, MemberId> = self
            .messages
            .iter()
            .filter(|m| !m.message.content.is_action())
            .map(|m| (m.id(), m.message.author))
            .collect();

        // Process action messages in timestamp order (messages are already sorted)
        for msg in &self.messages {
            let actor = msg.message.author;

            // Skip non-action messages
            if !msg.message.content.is_action() {
                continue;
            }

            // Decode the action content - either from public data or decrypted bytes
            let action = match &msg.message.content {
                RoomMessageBody::Public { .. } => {
                    // Public action - decode directly
                    match msg.message.content.decode_content() {
                        Some(DecodedContent::Action(action)) => action,
                        _ => continue,
                    }
                }
                RoomMessageBody::Private { .. } => {
                    // Private action - use provided decrypted content
                    let msg_id = msg.id();
                    if let Some(plaintext) = decrypted_content.get(&msg_id) {
                        match ActionContentV1::decode(plaintext) {
                            Ok(action) => action,
                            Err(_) => continue,
                        }
                    } else {
                        // No decrypted content provided - skip this action
                        continue;
                    }
                }
            };

            let target = &action.target;

            match action.action_type {
                ACTION_TYPE_EDIT => {
                    // Only the original author can edit their message
                    if let Some(&original_author) = message_authors.get(target) {
                        if actor == original_author {
                            // Don't allow editing deleted messages
                            if !self.actions_state.deleted.contains(target) {
                                if let Some(payload) = action.edit_payload() {
                                    self.actions_state
                                        .edited_content
                                        .insert(target.clone(), payload.new_text);
                                }
                            }
                        }
                    }
                }
                ACTION_TYPE_DELETE => {
                    // Only the original author can delete their message
                    if let Some(&original_author) = message_authors.get(target) {
                        if actor == original_author {
                            self.actions_state.deleted.insert(target.clone());
                            // Also remove any edited content for deleted messages
                            self.actions_state.edited_content.remove(target);
                        }
                    }
                }
                ACTION_TYPE_REACTION => {
                    // Anyone can add reactions to non-deleted messages
                    if message_authors.contains_key(target)
                        && !self.actions_state.deleted.contains(target)
                    {
                        if let Some(payload) = action.reaction_payload() {
                            let reactions = self
                                .actions_state
                                .reactions
                                .entry(target.clone())
                                .or_default();
                            let reactors = reactions.entry(payload.emoji).or_default();
                            // Idempotent: only add if not already present
                            if !reactors.contains(&actor) {
                                reactors.push(actor);
                            }
                        }
                    }
                }
                ACTION_TYPE_REMOVE_REACTION => {
                    // Users can only remove their own reactions
                    if let Some(payload) = action.reaction_payload() {
                        if let Some(reactions) = self.actions_state.reactions.get_mut(target) {
                            if let Some(reactors) = reactions.get_mut(&payload.emoji) {
                                reactors.retain(|r| r != &actor);
                                // Clean up empty entries
                                if reactors.is_empty() {
                                    reactions.remove(&payload.emoji);
                                }
                            }
                            if reactions.is_empty() {
                                self.actions_state.reactions.remove(target);
                            }
                        }
                    }
                }
                _ => {
                    // Unknown action type - ignore for forward compatibility
                }
            }
        }
    }

    /// Check if a message has been edited
    pub fn is_edited(&self, message_id: &MessageId) -> bool {
        self.actions_state.edited_content.contains_key(message_id)
    }

    /// Check if a message has been deleted
    pub fn is_deleted(&self, message_id: &MessageId) -> bool {
        self.actions_state.deleted.contains(message_id)
    }

    /// Get the effective text content for a message (edited content if edited, original otherwise)
    /// Returns the text content as a string, or None if the message is encrypted/undecodable
    pub fn effective_text(&self, message: &AuthorizedMessageV1) -> Option<String> {
        let id = message.id();
        // Check if there's edited content first
        if let Some(edited_text) = self.actions_state.edited_content.get(&id) {
            return Some(edited_text.clone());
        }
        // Otherwise return the original content's text
        message.message.content.as_public_string()
    }

    /// Get reactions for a message
    pub fn reactions(&self, message_id: &MessageId) -> Option<&HashMap<String, Vec<MemberId>>> {
        self.actions_state.reactions.get(message_id)
    }

    /// Get all non-deleted, non-action messages for display
    pub fn display_messages(&self) -> impl Iterator<Item = &AuthorizedMessageV1> {
        self.messages.iter().filter(|m| {
            !m.message.content.is_action() && !self.actions_state.deleted.contains(&m.id())
        })
    }
}

/// Message body that can be either public or private (encrypted).
///
/// Content is opaque to the contract - interpretation happens client-side.
/// This design enables adding new content types without contract redeployment.
///
/// # Content Types
/// - `content_type = 1`: Text message (TextContentV1)
/// - `content_type = 2`: Action on another message (ActionContentV1)
/// - `content_type = 3`: Reply to another message (ReplyContentV1)
/// - `content_type = 4`: Room event like join/leave (EventContentV1)
///   - Allowed as Public even in private rooms (contains no sensitive content)
///   - Old clients display as "[Unsupported message type 4.1 - please upgrade]"
/// - Future types can be added without contract changes
///
/// # Extensibility
/// - New content types: Just use a new content_type number
/// - New action types: Just use a new action_type number within ActionContentV1
/// - New fields: Add to content structs (old clients ignore unknown fields)
/// - Breaking changes: Bump content_version
/// # Do NOT apply `serde_bytes` to `data` / `ciphertext`
///
/// Both are bare `Vec<u8>`, so like `ActionContentV1::payload` before
/// freenet/river#443 they serialize as a CBOR array of integers (~2 bytes per
/// byte). That looks like the same easy win, and it is NOT: these fields live
/// inside `MessageV1`, which `verify_struct` RE-SERIALIZES to check the
/// signature (`AuthorizedMessageV1::verify`). Changing their encoding would
/// invalidate the signature of **every existing message in every room** —
/// unlike `ActionContentV1`, which is pre-encoded into these opaque bytes and
/// is therefore outside the signed representation.
///
/// The #443 fix was safe precisely because it stopped at that boundary. If the
/// on-wire size of message bodies ever needs to shrink, it requires a versioned
/// migration, not a serde attribute.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum RoomMessageBody {
    /// Public (unencrypted) message
    Public {
        /// Content type identifier (see content module for constants)
        content_type: u32,
        /// Version of the content format
        content_version: u32,
        /// CBOR-encoded content bytes.
        ///
        /// Do NOT add `serde(with = "serde_bytes")` — see the type-level note.
        data: Vec<u8>,
    },
    /// Private (encrypted) message
    Private {
        /// Content type identifier (see content module for constants)
        content_type: u32,
        /// Version of the content format
        content_version: u32,
        /// Encrypted CBOR-encoded content.
        ///
        /// Do NOT add `serde(with = "serde_bytes")` — see the type-level note.
        ciphertext: Vec<u8>,
        /// Nonce used for encryption
        nonce: [u8; 12],
        /// Version of the room secret used for encryption
        secret_version: SecretVersion,
    },
}

impl RoomMessageBody {
    /// Create a new public text message
    pub fn public(text: String) -> Self {
        use crate::room_state::content::{TextContentV1, CONTENT_TYPE_TEXT, TEXT_CONTENT_VERSION};
        let content = TextContentV1::new(text);
        Self::Public {
            content_type: CONTENT_TYPE_TEXT,
            content_version: TEXT_CONTENT_VERSION,
            data: content.encode(),
        }
    }

    /// Create a join event message
    pub fn join_event() -> Self {
        use crate::room_state::content::{
            EventContentV1, CONTENT_TYPE_EVENT, EVENT_CONTENT_VERSION,
        };
        let content = EventContentV1::join();
        Self::Public {
            content_type: CONTENT_TYPE_EVENT,
            content_version: EVENT_CONTENT_VERSION,
            data: content.encode(),
        }
    }

    /// Create a new public message with raw content
    pub fn public_raw(content_type: u32, content_version: u32, data: Vec<u8>) -> Self {
        Self::Public {
            content_type,
            content_version,
            data,
        }
    }

    /// Create a new private message
    pub fn private(
        content_type: u32,
        content_version: u32,
        ciphertext: Vec<u8>,
        nonce: [u8; 12],
        secret_version: SecretVersion,
    ) -> Self {
        Self::Private {
            content_type,
            content_version,
            ciphertext,
            nonce,
            secret_version,
        }
    }

    /// Create a private text message (convenience method)
    pub fn private_text(
        ciphertext: Vec<u8>,
        nonce: [u8; 12],
        secret_version: SecretVersion,
    ) -> Self {
        use crate::room_state::content::{CONTENT_TYPE_TEXT, TEXT_CONTENT_VERSION};
        Self::Private {
            content_type: CONTENT_TYPE_TEXT,
            content_version: TEXT_CONTENT_VERSION,
            ciphertext,
            nonce,
            secret_version,
        }
    }

    /// Create an edit action (public)
    pub fn edit(target: MessageId, new_text: String) -> Self {
        use crate::room_state::content::{
            ActionContentV1, ACTION_CONTENT_VERSION, CONTENT_TYPE_ACTION,
        };
        let action = ActionContentV1::edit(target, new_text);
        Self::Public {
            content_type: CONTENT_TYPE_ACTION,
            content_version: ACTION_CONTENT_VERSION,
            data: action.encode(),
        }
    }

    /// Create a delete action (public)
    pub fn delete(target: MessageId) -> Self {
        use crate::room_state::content::{
            ActionContentV1, ACTION_CONTENT_VERSION, CONTENT_TYPE_ACTION,
        };
        let action = ActionContentV1::delete(target);
        Self::Public {
            content_type: CONTENT_TYPE_ACTION,
            content_version: ACTION_CONTENT_VERSION,
            data: action.encode(),
        }
    }

    /// Create a reaction action (public)
    pub fn reaction(target: MessageId, emoji: String) -> Self {
        use crate::room_state::content::{
            ActionContentV1, ACTION_CONTENT_VERSION, CONTENT_TYPE_ACTION,
        };
        let action = ActionContentV1::reaction(target, emoji);
        Self::Public {
            content_type: CONTENT_TYPE_ACTION,
            content_version: ACTION_CONTENT_VERSION,
            data: action.encode(),
        }
    }

    /// Create a remove reaction action (public)
    pub fn remove_reaction(target: MessageId, emoji: String) -> Self {
        use crate::room_state::content::{
            ActionContentV1, ACTION_CONTENT_VERSION, CONTENT_TYPE_ACTION,
        };
        let action = ActionContentV1::remove_reaction(target, emoji);
        Self::Public {
            content_type: CONTENT_TYPE_ACTION,
            content_version: ACTION_CONTENT_VERSION,
            data: action.encode(),
        }
    }

    /// Create a public reply message
    pub fn reply(
        text: String,
        target_message_id: MessageId,
        target_author_name: String,
        target_content_preview: String,
    ) -> Self {
        use crate::room_state::content::{
            ReplyContentV1, CONTENT_TYPE_REPLY, REPLY_CONTENT_VERSION,
        };
        let reply = ReplyContentV1::new(
            text,
            target_message_id,
            target_author_name,
            target_content_preview,
        );
        Self::Public {
            content_type: CONTENT_TYPE_REPLY,
            content_version: REPLY_CONTENT_VERSION,
            data: reply.encode(),
        }
    }

    /// Create a private action message (encrypted)
    ///
    /// Use this for any action (edit, delete, reaction, remove_reaction) in a private room.
    /// The caller should:
    /// 1. Create the ActionContentV1 (e.g., `ActionContentV1::edit(target, new_text)`)
    /// 2. Encode it: `action.encode()`
    /// 3. Encrypt the bytes with the room secret
    /// 4. Pass the ciphertext here
    pub fn private_action(
        ciphertext: Vec<u8>,
        nonce: [u8; 12],
        secret_version: SecretVersion,
    ) -> Self {
        use crate::room_state::content::{ACTION_CONTENT_VERSION, CONTENT_TYPE_ACTION};
        Self::Private {
            content_type: CONTENT_TYPE_ACTION,
            content_version: ACTION_CONTENT_VERSION,
            ciphertext,
            nonce,
            secret_version,
        }
    }

    /// Check if this is a public message
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public { .. })
    }

    /// Check if this is a private message
    pub fn is_private(&self) -> bool {
        matches!(self, Self::Private { .. })
    }

    /// Get the content type
    pub fn content_type(&self) -> u32 {
        match self {
            Self::Public { content_type, .. } | Self::Private { content_type, .. } => *content_type,
        }
    }

    /// Get the content version
    pub fn content_version(&self) -> u32 {
        match self {
            Self::Public {
                content_version, ..
            }
            | Self::Private {
                content_version, ..
            } => *content_version,
        }
    }

    /// Check if this is an action message (content_type = ACTION)
    pub fn is_action(&self) -> bool {
        use crate::room_state::content::CONTENT_TYPE_ACTION;
        self.content_type() == CONTENT_TYPE_ACTION
    }

    /// Check if this is an event message (content_type = EVENT)
    pub fn is_event(&self) -> bool {
        use crate::room_state::content::CONTENT_TYPE_EVENT;
        self.content_type() == CONTENT_TYPE_EVENT
    }

    /// Decode the content (for public messages only)
    /// Returns None for private messages - decrypt first
    pub fn decode_content(&self) -> Option<crate::room_state::content::DecodedContent> {
        use crate::room_state::content::{
            ActionContentV1, DecodedContent, EventContentV1, ReplyContentV1, TextContentV1,
            CONTENT_TYPE_ACTION, CONTENT_TYPE_EVENT, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT,
        };
        match self {
            Self::Public {
                content_type,
                content_version,
                data,
            } => match *content_type {
                CONTENT_TYPE_TEXT => TextContentV1::decode(data).ok().map(DecodedContent::Text),
                CONTENT_TYPE_ACTION => ActionContentV1::decode(data)
                    .ok()
                    .map(DecodedContent::Action),
                CONTENT_TYPE_REPLY => ReplyContentV1::decode(data).ok().map(DecodedContent::Reply),
                CONTENT_TYPE_EVENT => EventContentV1::decode(data).ok().map(DecodedContent::Event),
                _ => Some(DecodedContent::Unknown {
                    content_type: *content_type,
                    content_version: *content_version,
                }),
            },
            Self::Private { .. } => None,
        }
    }

    /// Get the target message ID if this is an action
    pub fn target_id(&self) -> Option<MessageId> {
        use crate::room_state::content::{ActionContentV1, CONTENT_TYPE_ACTION};
        match self {
            Self::Public {
                content_type, data, ..
            } if *content_type == CONTENT_TYPE_ACTION => {
                ActionContentV1::decode(data).ok().map(|a| a.target)
            }
            _ => None,
        }
    }

    /// Get the content length for validation (contract uses this for size limits)
    pub fn content_len(&self) -> usize {
        match self {
            Self::Public { data, .. } => data.len(),
            Self::Private { ciphertext, .. } => ciphertext.len(),
        }
    }

    /// Exact [`Self::content_len`] of the body [`Self::public`] builds for
    /// `text` — or, with `encrypted`, of the private body the senders build
    /// by AES-256-GCM-sealing the encoded `TextContentV1`.
    ///
    /// Send gates and byte counters MUST use the `measure_*` functions, not
    /// `text.len()`: the contract validates encoded content bytes (CBOR
    /// framing, plus the AEAD tag in private rooms), so a raw-text gate
    /// passes messages the contract then silently prunes (freenet/river#430,
    /// the "message was lost" reports).
    pub fn measure_text(text: &str, encrypted: bool) -> usize {
        use crate::room_state::content::TextContentV1;
        let plain = TextContentV1::new(text.to_owned()).encode().len();
        Self::with_encryption_overhead(plain, encrypted)
    }

    /// Exact [`Self::content_len`] of the body [`Self::reply`] builds — or,
    /// with `encrypted`, of the private reply body (encrypted encoded
    /// `ReplyContentV1`). Reply bodies embed the quoted author name and
    /// content preview, so their overhead is much larger than plain text.
    pub fn measure_reply(
        text: &str,
        target_message_id: MessageId,
        target_author_name: &str,
        target_content_preview: &str,
        encrypted: bool,
    ) -> usize {
        use crate::room_state::content::ReplyContentV1;
        let plain = ReplyContentV1::new(
            text.to_owned(),
            target_message_id,
            target_author_name.to_owned(),
            target_content_preview.to_owned(),
        )
        .encode()
        .len();
        Self::with_encryption_overhead(plain, encrypted)
    }

    /// Exact [`Self::content_len`] of the body [`Self::edit`] builds — or,
    /// with `encrypted`, of the private edit body (encrypted encoded
    /// `ActionContentV1`).
    pub fn measure_edit(target: MessageId, new_text: &str, encrypted: bool) -> usize {
        use crate::room_state::content::ActionContentV1;
        let plain = ActionContentV1::edit(target, new_text.to_owned())
            .encode()
            .len();
        Self::with_encryption_overhead(plain, encrypted)
    }

    fn with_encryption_overhead(plain_len: usize, encrypted: bool) -> usize {
        if encrypted {
            plain_len + ENCRYPTION_TAG_OVERHEAD
        } else {
            plain_len
        }
    }

    /// Get the secret version (if private)
    pub fn secret_version(&self) -> Option<SecretVersion> {
        match self {
            Self::Public { .. } => None,
            Self::Private { secret_version, .. } => Some(*secret_version),
        }
    }

    /// Get a string representation for display purposes
    pub fn to_string_lossy(&self) -> String {
        match self {
            Self::Public { .. } => {
                if let Some(decoded) = self.decode_content() {
                    decoded.to_display_string()
                } else {
                    "[Failed to decode message]".to_string()
                }
            }
            Self::Private {
                ciphertext,
                secret_version,
                ..
            } => {
                format!(
                    "[Encrypted message: {} bytes, v{}]",
                    ciphertext.len(),
                    secret_version
                )
            }
        }
    }

    /// Try to get the public plaintext, returns None if private or not a text message
    pub fn as_public_string(&self) -> Option<String> {
        self.decode_content()
            .and_then(|c| c.as_text().map(|s| s.to_string()))
    }
}

impl fmt::Display for RoomMessageBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_string_lossy())
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct MessageV1 {
    pub room_owner: MemberId,
    pub author: MemberId,
    pub time: SystemTime,
    pub content: RoomMessageBody,
}

impl Default for MessageV1 {
    fn default() -> Self {
        Self {
            room_owner: MemberId(FastHash(0)),
            author: MemberId(FastHash(0)),
            time: SystemTime::UNIX_EPOCH,
            content: RoomMessageBody::public(String::new()),
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedMessageV1 {
    pub message: MessageV1,
    pub signature: Signature,
}

impl fmt::Debug for AuthorizedMessageV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedMessage")
            .field("message", &self.message)
            .field(
                "signature",
                &format_args!("{}", truncated_base64(self.signature.to_bytes())),
            )
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd)]
pub struct MessageId(pub FastHash);

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl AuthorizedMessageV1 {
    pub fn new(message: MessageV1, signing_key: &SigningKey) -> Self {
        Self {
            message: message.clone(),
            signature: sign_struct(&message, signing_key),
        }
    }

    /// Create an AuthorizedMessageV1 with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(message: MessageV1, signature: Signature) -> Self {
        Self { message, signature }
    }

    pub fn validate(
        &self,
        verifying_key: &VerifyingKey,
    ) -> Result<(), ed25519_dalek::SignatureError> {
        verify_struct(&self.message, &self.signature, verifying_key)
    }

    pub fn id(&self) -> MessageId {
        MessageId(fast_hash(&self.signature.to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use std::time::Duration;

    fn create_test_message(owner_id: MemberId, author_id: MemberId) -> MessageV1 {
        MessageV1 {
            room_owner: owner_id,
            author: author_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Test message".to_string()),
        }
    }

    #[test]
    fn test_messages_v1_default() {
        let default_messages = MessagesV1::default();
        assert!(default_messages.messages.is_empty());
    }

    /// Full-stack pin for freenet/river#443's backward compatibility.
    ///
    /// The unit tests in `content.rs` prove `ActionContentV1::decode` accepts
    /// the legacy array-of-integers payload, but the behaviour that actually
    /// matters is that `rebuild_actions_state` — the entry point the contract
    /// and every client run after `apply_delta` — still SURFACES those
    /// pre-existing edits, deletes and reactions. A change to which decode
    /// path that routes through would break every already-stored action while
    /// the unit tests stayed green.
    #[test]
    fn legacy_encoded_actions_still_render_through_rebuild_actions_state() {
        use crate::room_state::content::{
            ActionContentV1, ACTION_TYPE_DELETE, ACTION_TYPE_EDIT, ACTION_TYPE_REACTION,
            CONTENT_TYPE_ACTION,
        };

        /// Pre-#443 shape: bare `Vec<u8>` -> CBOR array of integers.
        #[derive(Serialize)]
        struct LegacyAction {
            action_type: u32,
            target: MessageId,
            payload: Vec<u8>,
        }

        fn legacy_action_body(action: &ActionContentV1) -> RoomMessageBody {
            let mut data = Vec::new();
            ciborium::into_writer(
                &LegacyAction {
                    action_type: action.action_type,
                    target: action.target.clone(),
                    payload: action.payload.clone(),
                },
                &mut data,
            )
            .expect("serialize legacy action");
            RoomMessageBody::public_raw(CONTENT_TYPE_ACTION, 1, data)
        }

        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId::from(&signing_key.verifying_key());

        let original = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: author_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("original text".to_string()),
            },
            &signing_key,
        );
        let target = original.id();

        let push_action = |messages: &mut MessagesV1, action: ActionContentV1| {
            messages.messages.push(AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: author_id,
                    time: SystemTime::now() + Duration::from_secs(1),
                    content: legacy_action_body(&action),
                },
                &signing_key,
            ));
        };

        // --- a legacy EDIT must still render ---
        let mut messages = MessagesV1 {
            messages: vec![original.clone()],
            ..Default::default()
        };
        push_action(
            &mut messages,
            ActionContentV1 {
                action_type: ACTION_TYPE_EDIT,
                target: target.clone(),
                payload: ActionContentV1::edit(target.clone(), "edited text".to_string()).payload,
            },
        );
        messages.rebuild_actions_state();
        assert!(
            messages.is_edited(&target),
            "a pre-#443 stored edit must still be seen as an edit"
        );
        assert_eq!(
            messages.effective_text(&original).as_deref(),
            Some("edited text"),
            "a pre-#443 stored edit must still render its new text"
        );

        // --- a legacy REACTION (emoji -> bytes >= 0x80) must still render ---
        let mut messages = MessagesV1 {
            messages: vec![original.clone()],
            ..Default::default()
        };
        push_action(
            &mut messages,
            ActionContentV1 {
                action_type: ACTION_TYPE_REACTION,
                target: target.clone(),
                payload: ActionContentV1::reaction(target.clone(), "👍".to_string()).payload,
            },
        );
        messages.rebuild_actions_state();
        assert!(
            messages
                .actions_state
                .reactions
                .get(&target)
                .is_some_and(|r| r.contains_key("👍")),
            "a pre-#443 stored emoji reaction must still render"
        );

        // --- a legacy DELETE (empty payload) must still apply ---
        let mut messages = MessagesV1 {
            messages: vec![original.clone()],
            ..Default::default()
        };
        push_action(
            &mut messages,
            ActionContentV1 {
                action_type: ACTION_TYPE_DELETE,
                target: target.clone(),
                payload: Vec::new(),
            },
        );
        messages.rebuild_actions_state();
        assert!(
            messages.is_deleted(&target),
            "a pre-#443 stored delete must still apply"
        );
    }

    #[test]
    fn test_authorized_message_v1_debug() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message = create_test_message(owner_id, author_id);
        let authorized_message = AuthorizedMessageV1::new(message, &signing_key);

        let debug_output = format!("{:?}", authorized_message);
        assert!(debug_output.contains("AuthorizedMessage"));
        assert!(debug_output.contains("message"));
        assert!(debug_output.contains("signature"));
    }

    #[test]
    fn test_authorized_message_new_and_validate() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message = create_test_message(owner_id, author_id);
        let authorized_message = AuthorizedMessageV1::new(message.clone(), &signing_key);

        assert_eq!(authorized_message.message, message);
        assert!(authorized_message.validate(&verifying_key).is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        assert!(authorized_message.validate(&wrong_key).is_err());

        // Test with tampered message
        let mut tampered_message = authorized_message.clone();
        tampered_message.message.content = RoomMessageBody::public("Tampered content".to_string());
        assert!(tampered_message.validate(&verifying_key).is_err());
    }

    #[test]
    fn test_message_id() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message = create_test_message(owner_id, author_id);
        let authorized_message = AuthorizedMessageV1::new(message, &signing_key);

        let id1 = authorized_message.id();
        let id2 = authorized_message.id();

        assert_eq!(id1, id2);

        // Test that different messages have different IDs
        let message2 = create_test_message(owner_id, author_id);
        let authorized_message2 = AuthorizedMessageV1::new(message2, &signing_key);
        assert_ne!(authorized_message.id(), authorized_message2.id());
    }

    #[test]
    fn test_messages_verify() {
        // Generate a new signing key and its corresponding verifying key for the owner
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::from(&owner_verifying_key);

        // Generate a signing key for the author
        let author_signing_key = SigningKey::generate(&mut OsRng);
        let author_verifying_key = author_signing_key.verifying_key();
        let author_id = MemberId::from(&author_verifying_key);

        // Create a test message and authorize it with the author's signing key
        let message = create_test_message(owner_id, author_id);
        let authorized_message = AuthorizedMessageV1::new(message, &author_signing_key);

        // Create a Messages struct with the authorized message
        let messages = MessagesV1 {
            messages: vec![authorized_message],
            ..Default::default()
        };

        // Set up a parent room_state (ChatRoomState) with the author as a member
        let mut parent_state = ChatRoomStateV1::default();
        let author_member = crate::room_state::member::Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: author_verifying_key,
        };
        let authorized_author =
            crate::room_state::member::AuthorizedMember::new(author_member, &owner_signing_key);
        parent_state.members.members = vec![authorized_author];

        // Set up parameters for verification
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Verify that a valid message passes verification
        assert!(
            messages.verify(&parent_state, &parameters).is_ok(),
            "Valid messages should pass verification: {:?}",
            messages.verify(&parent_state, &parameters)
        );

        // Test with invalid signature
        let mut invalid_messages = messages.clone();
        invalid_messages.messages[0].signature = Signature::from_bytes(&[0; 64]); // Replace with an invalid signature
        assert!(
            invalid_messages.verify(&parent_state, &parameters).is_err(),
            "Messages with invalid signature should fail verification"
        );

        // Test with non-existent author
        let non_existent_author_id =
            MemberId::from(&SigningKey::generate(&mut OsRng).verifying_key());
        let invalid_message = create_test_message(owner_id, non_existent_author_id);
        let invalid_authorized_message =
            AuthorizedMessageV1::new(invalid_message, &author_signing_key);
        let invalid_messages = MessagesV1 {
            messages: vec![invalid_authorized_message],
            ..Default::default()
        };
        assert!(
            invalid_messages.verify(&parent_state, &parameters).is_err(),
            "Messages with non-existent author should fail verification"
        );
    }

    #[test]
    fn test_messages_summarize() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        let message1 = create_test_message(owner_id, author_id);
        let message2 = create_test_message(owner_id, author_id);

        let authorized_message1 = AuthorizedMessageV1::new(message1, &signing_key);
        let authorized_message2 = AuthorizedMessageV1::new(message2, &signing_key);

        let messages = MessagesV1 {
            messages: vec![authorized_message1.clone(), authorized_message2.clone()],
            ..Default::default()
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: signing_key.verifying_key(),
        };

        let summary = messages.summarize(&parent_state, &parameters);
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0], authorized_message1.id());
        assert_eq!(summary[1], authorized_message2.id());

        // Test empty messages
        let empty_messages = MessagesV1::default();
        let empty_summary = empty_messages.summarize(&parent_state, &parameters);
        assert!(empty_summary.is_empty());
    }

    #[test]
    fn test_messages_delta() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let owner_id = MemberId(FastHash(0));
        let author_id = MemberId(FastHash(1));

        // Use distinct timestamps to ensure unique message IDs
        let base = SystemTime::now();
        let message1 = MessageV1 {
            room_owner: owner_id,
            author: author_id,
            time: base,
            content: RoomMessageBody::public("Message 1".to_string()),
        };
        let message2 = MessageV1 {
            room_owner: owner_id,
            author: author_id,
            time: base + Duration::from_millis(1),
            content: RoomMessageBody::public("Message 2".to_string()),
        };
        let message3 = MessageV1 {
            room_owner: owner_id,
            author: author_id,
            time: base + Duration::from_millis(2),
            content: RoomMessageBody::public("Message 3".to_string()),
        };

        let authorized_message1 = AuthorizedMessageV1::new(message1, &signing_key);
        let authorized_message2 = AuthorizedMessageV1::new(message2, &signing_key);
        let authorized_message3 = AuthorizedMessageV1::new(message3, &signing_key);

        let messages = MessagesV1 {
            messages: vec![
                authorized_message1.clone(),
                authorized_message2.clone(),
                authorized_message3.clone(),
            ],
            ..Default::default()
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: signing_key.verifying_key(),
        };

        // Test with partial old summary
        let old_summary = vec![authorized_message1.id(), authorized_message2.id()];
        let delta = messages
            .delta(&parent_state, &parameters, &old_summary)
            .unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0], authorized_message3);

        // Test with empty old summary
        let empty_summary: Vec<MessageId> = vec![];
        let full_delta = messages
            .delta(&parent_state, &parameters, &empty_summary)
            .unwrap();
        assert_eq!(full_delta.len(), 3);
        assert_eq!(full_delta, messages.messages);

        // Test with full old summary (no changes)
        let full_summary = vec![
            authorized_message1.id(),
            authorized_message2.id(),
            authorized_message3.id(),
        ];
        let no_delta = messages.delta(&parent_state, &parameters, &full_summary);
        assert!(no_delta.is_none());
    }

    #[test]
    fn test_messages_apply_delta() {
        // Setup
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::from(&owner_verifying_key);

        let author_signing_key = SigningKey::generate(&mut OsRng);
        let author_verifying_key = author_signing_key.verifying_key();
        let author_id = MemberId::from(&author_verifying_key);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_recent_messages = 3;
        parent_state.configuration.configuration.max_message_size = 100;
        parent_state.members.members = vec![crate::room_state::member::AuthorizedMember {
            member: crate::room_state::member::Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: author_verifying_key,
            },
            signature: owner_signing_key.try_sign(&[0; 32]).unwrap(),
        }];

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Create messages
        let create_message = |time: SystemTime| {
            let message = MessageV1 {
                room_owner: owner_id,
                author: author_id,
                time,
                content: RoomMessageBody::public("Test message".to_string()),
            };
            AuthorizedMessageV1::new(message, &author_signing_key)
        };

        let now = SystemTime::now();
        let message1 = create_message(now - Duration::from_secs(3));
        let message2 = create_message(now - Duration::from_secs(2));
        let message3 = create_message(now - Duration::from_secs(1));
        let message4 = create_message(now);

        // Initial room_state with 2 messages
        let mut messages = MessagesV1 {
            messages: vec![message1.clone(), message2.clone()],
            ..Default::default()
        };

        // Apply delta with 2 new messages
        let delta = vec![message3.clone(), message4.clone()];
        assert!(messages
            .apply_delta(&parent_state, &parameters, &Some(delta))
            .is_ok());

        // Check results
        assert_eq!(
            messages.messages.len(),
            3,
            "Should have 3 messages after applying delta"
        );
        assert!(
            !messages.messages.contains(&message1),
            "Oldest message should be removed"
        );
        assert!(
            messages.messages.contains(&message2),
            "Second oldest message should be retained"
        );
        assert!(
            messages.messages.contains(&message3),
            "New message should be added"
        );
        assert!(
            messages.messages.contains(&message4),
            "Newest message should be added"
        );

        // Apply delta with an older message
        let old_message = create_message(now - Duration::from_secs(4));
        let delta = vec![old_message.clone()];
        assert!(messages
            .apply_delta(&parent_state, &parameters, &Some(delta))
            .is_ok());

        // Check results
        assert_eq!(messages.messages.len(), 3, "Should still have 3 messages");
        assert!(
            !messages.messages.contains(&old_message),
            "Older message should not be added"
        );
        assert!(
            messages.messages.contains(&message2),
            "Message2 should be retained"
        );
        assert!(
            messages.messages.contains(&message3),
            "Message3 should be retained"
        );
        assert!(
            messages.messages.contains(&message4),
            "Newest message should be retained"
        );
    }

    #[test]
    fn test_oversized_message_filtered_by_apply_delta() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        let author_sk = SigningKey::generate(&mut OsRng);
        let author_vk = author_sk.verifying_key();
        let author_id = MemberId::from(&author_vk);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_message_size = 50;
        parent_state.configuration.configuration.max_recent_messages = 10;
        parent_state.members.members = vec![crate::room_state::member::AuthorizedMember {
            member: crate::room_state::member::Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: author_vk,
            },
            signature: owner_sk.try_sign(&[0; 32]).unwrap(),
        }];

        let parameters = ChatRoomParametersV1 { owner: owner_vk };

        // Create a normal-sized message and an oversized message
        let small_msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: author_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("short".to_string()),
            },
            &author_sk,
        );
        let big_msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: author_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("x".repeat(100)),
            },
            &author_sk,
        );

        assert!(small_msg.message.content.content_len() <= 50);
        assert!(big_msg.message.content.content_len() > 50);

        let mut messages = MessagesV1::default();
        let delta = vec![small_msg.clone(), big_msg.clone()];
        assert!(messages
            .apply_delta(&parent_state, &parameters, &Some(delta))
            .is_ok());

        assert_eq!(
            messages.messages.len(),
            1,
            "Only small message should survive"
        );
        assert!(messages.messages.contains(&small_msg));
        assert!(
            !messages.messages.contains(&big_msg),
            "Oversized message should be filtered"
        );
    }

    #[test]
    fn test_message_author_preservation_across_users() {
        // Create two users
        let user1_sk = SigningKey::generate(&mut OsRng);
        let user1_vk = user1_sk.verifying_key();
        let user1_id = MemberId::from(&user1_vk);

        let user2_sk = SigningKey::generate(&mut OsRng);
        let user2_vk = user2_sk.verifying_key();
        let user2_id = MemberId::from(&user2_vk);

        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        println!("User1 ID: {}", user1_id);
        println!("User2 ID: {}", user2_id);
        println!("Owner ID: {}", owner_id);

        // Create messages from different users
        let msg1 = MessageV1 {
            room_owner: owner_id,
            author: user1_id,
            content: RoomMessageBody::public("Message from user1".to_string()),
            time: SystemTime::now(),
        };

        let msg2 = MessageV1 {
            room_owner: owner_id,
            author: user2_id,
            content: RoomMessageBody::public("Message from user2".to_string()),
            time: SystemTime::now() + Duration::from_secs(1),
        };

        let auth_msg1 = AuthorizedMessageV1::new(msg1.clone(), &user1_sk);
        let auth_msg2 = AuthorizedMessageV1::new(msg2.clone(), &user2_sk);

        // Create a messages state with both messages
        let messages = MessagesV1 {
            messages: vec![auth_msg1.clone(), auth_msg2.clone()],
            ..Default::default()
        };

        // Verify authors are preserved
        assert_eq!(messages.messages.len(), 2);

        let stored_msg1 = &messages.messages[0];
        let stored_msg2 = &messages.messages[1];

        assert_eq!(
            stored_msg1.message.author, user1_id,
            "Message 1 author should be user1, but got {}",
            stored_msg1.message.author
        );
        assert_eq!(
            stored_msg2.message.author, user2_id,
            "Message 2 author should be user2, but got {}",
            stored_msg2.message.author
        );

        // Test that author IDs are different
        assert_ne!(user1_id, user2_id, "User IDs should be different");

        // Test Display implementation
        let user1_id_str = user1_id.to_string();
        let user2_id_str = user2_id.to_string();

        println!("User1 ID string: {}", user1_id_str);
        println!("User2 ID string: {}", user2_id_str);

        assert_ne!(
            user1_id_str, user2_id_str,
            "User ID strings should be different"
        );
    }

    #[test]
    fn test_edit_action() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId::from(&verifying_key);
        let author_id = owner_id;

        // Create original message
        let original_msg = MessageV1 {
            room_owner: owner_id,
            author: author_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Original content".to_string()),
        };
        let auth_original = AuthorizedMessageV1::new(original_msg, &signing_key);
        let original_id = auth_original.id();

        // Create edit action
        let edit_msg = MessageV1 {
            room_owner: owner_id,
            author: author_id,
            time: SystemTime::now() + Duration::from_secs(1),
            content: RoomMessageBody::edit(original_id.clone(), "Edited content".to_string()),
        };
        let auth_edit = AuthorizedMessageV1::new(edit_msg, &signing_key);

        // Create messages state and rebuild
        let mut messages = MessagesV1 {
            messages: vec![auth_original.clone(), auth_edit],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // Verify edit was applied
        assert!(messages.is_edited(&original_id));
        let effective = messages.effective_text(&auth_original);
        assert_eq!(effective, Some("Edited content".to_string()));

        // Verify display_messages still shows the original message
        let display: Vec<_> = messages.display_messages().collect();
        assert_eq!(display.len(), 1);
    }

    #[test]
    fn test_edit_by_non_author_ignored() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        let other_sk = SigningKey::generate(&mut OsRng);
        let other_id = MemberId::from(&other_sk.verifying_key());

        // Create message by owner
        let original_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Original content".to_string()),
        };
        let auth_original = AuthorizedMessageV1::new(original_msg, &owner_sk);
        let original_id = auth_original.id();

        // Create edit action by OTHER user (should be ignored)
        let edit_msg = MessageV1 {
            room_owner: owner_id,
            author: other_id,
            time: SystemTime::now() + Duration::from_secs(1),
            content: RoomMessageBody::edit(original_id.clone(), "Hacked content".to_string()),
        };
        let auth_edit = AuthorizedMessageV1::new(edit_msg, &other_sk);

        let mut messages = MessagesV1 {
            messages: vec![auth_original.clone(), auth_edit],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // Edit should be ignored - original content preserved
        assert!(!messages.is_edited(&original_id));
        let effective = messages.effective_text(&auth_original);
        assert_eq!(effective, Some("Original content".to_string()));
    }

    #[test]
    fn test_delete_action() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId::from(&verifying_key);

        // Create original message
        let original_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Will be deleted".to_string()),
        };
        let auth_original = AuthorizedMessageV1::new(original_msg, &signing_key);
        let original_id = auth_original.id();

        // Create delete action
        let delete_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now() + Duration::from_secs(1),
            content: RoomMessageBody::delete(original_id.clone()),
        };
        let auth_delete = AuthorizedMessageV1::new(delete_msg, &signing_key);

        let mut messages = MessagesV1 {
            messages: vec![auth_original, auth_delete],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // Verify message is deleted
        assert!(messages.is_deleted(&original_id));

        // Verify display_messages excludes deleted message
        let display: Vec<_> = messages.display_messages().collect();
        assert_eq!(display.len(), 0);
    }

    #[test]
    fn test_reaction_action() {
        let user1_sk = SigningKey::generate(&mut OsRng);
        let user1_id = MemberId::from(&user1_sk.verifying_key());

        let user2_sk = SigningKey::generate(&mut OsRng);
        let user2_id = MemberId::from(&user2_sk.verifying_key());

        let owner_id = user1_id;

        // Create original message
        let original_msg = MessageV1 {
            room_owner: owner_id,
            author: user1_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("React to me!".to_string()),
        };
        let auth_original = AuthorizedMessageV1::new(original_msg, &user1_sk);
        let original_id = auth_original.id();

        // Create reaction from user2
        let reaction_msg = MessageV1 {
            room_owner: owner_id,
            author: user2_id,
            time: SystemTime::now() + Duration::from_secs(1),
            content: RoomMessageBody::reaction(original_id.clone(), "👍".to_string()),
        };
        let auth_reaction = AuthorizedMessageV1::new(reaction_msg, &user2_sk);

        // Create another reaction from user1
        let reaction_msg2 = MessageV1 {
            room_owner: owner_id,
            author: user1_id,
            time: SystemTime::now() + Duration::from_secs(2),
            content: RoomMessageBody::reaction(original_id.clone(), "👍".to_string()),
        };
        let auth_reaction2 = AuthorizedMessageV1::new(reaction_msg2, &user1_sk);

        let mut messages = MessagesV1 {
            messages: vec![auth_original, auth_reaction, auth_reaction2],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // Verify reactions
        let reactions = messages.reactions(&original_id).unwrap();
        let thumbs_up = reactions.get("👍").unwrap();
        assert_eq!(thumbs_up.len(), 2);
        assert!(thumbs_up.contains(&user1_id));
        assert!(thumbs_up.contains(&user2_id));
    }

    #[test]
    fn test_remove_reaction_action() {
        let user_sk = SigningKey::generate(&mut OsRng);
        let user_id = MemberId::from(&user_sk.verifying_key());
        let owner_id = user_id;

        // Create original message
        let original_msg = MessageV1 {
            room_owner: owner_id,
            author: user_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Test message".to_string()),
        };
        let auth_original = AuthorizedMessageV1::new(original_msg, &user_sk);
        let original_id = auth_original.id();

        // Add reaction
        let reaction_msg = MessageV1 {
            room_owner: owner_id,
            author: user_id,
            time: SystemTime::now() + Duration::from_secs(1),
            content: RoomMessageBody::reaction(original_id.clone(), "❤️".to_string()),
        };
        let auth_reaction = AuthorizedMessageV1::new(reaction_msg, &user_sk);

        // Remove reaction
        let remove_msg = MessageV1 {
            room_owner: owner_id,
            author: user_id,
            time: SystemTime::now() + Duration::from_secs(2),
            content: RoomMessageBody::remove_reaction(original_id.clone(), "❤️".to_string()),
        };
        let auth_remove = AuthorizedMessageV1::new(remove_msg, &user_sk);

        let mut messages = MessagesV1 {
            messages: vec![auth_original, auth_reaction, auth_remove],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // Verify reaction was removed
        assert!(messages.reactions(&original_id).is_none());
    }

    #[test]
    fn test_action_on_deleted_message_ignored() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId::from(&verifying_key);

        // Create original message
        let original_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Will be deleted".to_string()),
        };
        let auth_original = AuthorizedMessageV1::new(original_msg, &signing_key);
        let original_id = auth_original.id();

        // Delete it
        let delete_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now() + Duration::from_secs(1),
            content: RoomMessageBody::delete(original_id.clone()),
        };
        let auth_delete = AuthorizedMessageV1::new(delete_msg, &signing_key);

        // Try to edit deleted message (should be ignored)
        let edit_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now() + Duration::from_secs(2),
            content: RoomMessageBody::edit(original_id.clone(), "Too late!".to_string()),
        };
        let auth_edit = AuthorizedMessageV1::new(edit_msg, &signing_key);

        let mut messages = MessagesV1 {
            messages: vec![auth_original, auth_delete, auth_edit],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // Message should be deleted, edit should be ignored
        assert!(messages.is_deleted(&original_id));
        assert!(!messages.is_edited(&original_id));
    }

    #[test]
    fn test_display_messages_filters_actions() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let owner_id = MemberId::from(&verifying_key);

        // Create regular message
        let msg1 = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Hello".to_string()),
        };
        let auth_msg1 = AuthorizedMessageV1::new(msg1, &signing_key);
        let msg1_id = auth_msg1.id();

        // Create reaction (action message)
        let reaction_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now() + Duration::from_secs(1),
            content: RoomMessageBody::reaction(msg1_id, "👍".to_string()),
        };
        let auth_reaction = AuthorizedMessageV1::new(reaction_msg, &signing_key);

        // Create another regular message
        let msg2 = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now() + Duration::from_secs(2),
            content: RoomMessageBody::public("World".to_string()),
        };
        let auth_msg2 = AuthorizedMessageV1::new(msg2, &signing_key);

        let mut messages = MessagesV1 {
            messages: vec![auth_msg1, auth_reaction, auth_msg2],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // display_messages should only return regular messages, not actions
        let display: Vec<_> = messages.display_messages().collect();
        assert_eq!(display.len(), 2);
        assert_eq!(
            display[0].message.content.as_public_string(),
            Some("Hello".to_string())
        );
        assert_eq!(
            display[1].message.content.as_public_string(),
            Some("World".to_string())
        );
    }
}

#[cfg(test)]
mod measure_tests {
    use super::*;
    use crate::room_state::content::{
        CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT, REPLY_CONTENT_VERSION, TEXT_CONTENT_VERSION,
    };

    /// Text samples crossing CBOR length-prefix boundaries (23/24, 255/256
    /// bytes) and mixing multi-byte UTF-8 (the HostFat report: chars < limit
    /// but encoded bytes > limit).
    fn samples() -> Vec<String> {
        vec![
            String::new(),
            "a".repeat(1),
            "a".repeat(23),
            "a".repeat(24),
            "a".repeat(255),
            "a".repeat(256),
            "a".repeat(997),
            "a".repeat(998),
            "a".repeat(1000),
            "é".repeat(400),  // 800 bytes, 400 chars
            "🎉".repeat(200), // 800 bytes, 200 chars
            format!("{}é🎉", "a".repeat(990)),
        ]
    }

    fn target_id() -> MessageId {
        MessageId(FastHash(0x1234_5678_9abc_def0_u64 as i64))
    }

    #[test]
    fn measure_text_matches_public_body() {
        for text in samples() {
            let body = RoomMessageBody::public(text.clone());
            assert_eq!(
                RoomMessageBody::measure_text(&text, false),
                body.content_len(),
                "text bytes={} chars={}",
                text.len(),
                text.chars().count()
            );
        }
    }

    #[test]
    fn measure_reply_matches_reply_body() {
        let previews = ["", "short", &"préview🎉 ".repeat(10)];
        let authors = ["", "Alice", "HöstFat"];
        for text in samples() {
            for preview in previews {
                for author in authors {
                    let body = RoomMessageBody::reply(
                        text.clone(),
                        target_id(),
                        author.to_string(),
                        preview.to_string(),
                    );
                    assert_eq!(
                        RoomMessageBody::measure_reply(&text, target_id(), author, preview, false),
                        body.content_len(),
                        "text bytes={} author={:?} preview bytes={}",
                        text.len(),
                        author,
                        preview.len()
                    );
                }
            }
        }
    }

    #[test]
    fn measure_edit_matches_edit_body() {
        for text in samples() {
            let body = RoomMessageBody::edit(target_id(), text.clone());
            let measured = RoomMessageBody::measure_edit(target_id(), &text, false);
            assert_eq!(measured, body.content_len(), "text bytes={}", text.len());

            // Consistency alone let freenet/river#443 hide here for months:
            // this loop happily accepted a 1000-char edit measuring ~2070
            // bytes because it only compared the measure against the body.
            // Also assert MAGNITUDE, across every sample — which covers the
            // multi-byte UTF-8 and CBOR length-prefix boundary cases that the
            // ASCII-only #443 pins do not.
            assert!(
                measured <= text.len() + 80,
                "edit overhead must be a small constant: {measured} bytes for {} text bytes",
                text.len()
            );
        }
    }

    /// Pins the bug class this API exists to prevent: raw text within the
    /// default 1000-byte limit whose ENCODED content exceeds it. The old UI
    /// gate compared `text.len()` and let these through; the contract then
    /// silently pruned them ("a message was lost").
    #[test]
    fn raw_text_gate_undercounts_encoded_size() {
        let max = 1000;
        let text = "a".repeat(998); // 998 chars -> 1007 encoded (raw + 9)
        assert!(text.len() <= max);
        assert!(RoomMessageBody::measure_text(&text, false) > max);

        // A reply blows the budget far earlier because of embedded metadata.
        let reply_text = "a".repeat(900);
        assert!(reply_text.len() <= max);
        assert!(
            RoomMessageBody::measure_reply(
                &reply_text,
                target_id(),
                "Alice",
                &"p".repeat(100),
                false
            ) > max
        );
    }

    /// Regression pin for freenet/river#443 at the level the UI gate uses.
    ///
    /// `ActionContentV1::payload` used to serialize as a CBOR array of
    /// integers, costing ~2.1 bytes per ASCII character against a plain
    /// message's ~1.01. Against the default 1000-byte limit that capped edits
    /// at ~467 characters while sends allowed ~991, so a message could be sent
    /// and then never edited.
    ///
    /// The fix makes edit cost **proportional-parity** with send: the overhead
    /// is now a small CONSTANT (CBOR framing for `action_type` / `target` /
    /// the nested `EditPayload`), not a per-character multiplier. This test
    /// pins that property, which is the one that actually prevents the bug
    /// class from scaling with message length.
    ///
    /// NOTE: a residual constant gap remains — see
    /// `edit_overhead_over_send_is_a_small_constant`. It is ~54 bytes, so the
    /// longest editable message (~946 chars) is still slightly shorter than
    /// the longest sendable one (~991). Closing that fully would mean either
    /// shrinking the send budget or another wire-format change, so it is
    /// deliberately left as a documented, bounded residual rather than
    /// silently fixed here.
    #[test]
    fn edit_cost_is_not_proportional_to_length() {
        for n in [100usize, 400, 900] {
            let text = "a".repeat(n);
            let overhead = RoomMessageBody::measure_edit(target_id(), &text, false) - n;
            assert!(
                overhead < 80,
                "edit overhead must be a small constant, got {overhead} bytes for {n} chars"
            );
        }

        // The concrete user-facing win: a 900-char edit now fits the default
        // budget (it measured ~1866 bytes before the fix).
        let text = "a".repeat(900);
        assert!(RoomMessageBody::measure_edit(target_id(), &text, false) <= 1000);

        // NOTE: deliberately no `private - public == ENCRYPTION_TAG_OVERHEAD`
        // assertion here — both sides funnel through `with_encryption_overhead`,
        // so it holds by construction for ANY encoding and would inflate this
        // test's apparent coverage. The real pin is
        // `private_bodies::measure_edit_matches_private_body`, which compares
        // against an actually-encrypted body.
    }

    /// Pins the RESIDUAL of freenet/river#443 so it cannot silently grow back
    /// into a proportional cost. An edit of the same text costs a bounded
    /// constant more than sending it; if this constant creeps up, the
    /// send-but-cannot-edit window widens again.
    #[test]
    fn edit_overhead_over_send_is_a_small_constant() {
        let mut overheads = vec![];
        for n in [10usize, 100, 500, 900] {
            let text = "a".repeat(n);
            let send = RoomMessageBody::measure_text(&text, false);
            let edit = RoomMessageBody::measure_edit(target_id(), &text, false);
            assert!(
                edit > send,
                "an edit carries strictly more framing than a send"
            );
            overheads.push(edit - send);
        }

        let max_overhead = *overheads.iter().max().expect("non-empty");
        assert!(
            max_overhead <= 60,
            "edit-over-send overhead must stay a small constant, got {overheads:?}"
        );
        // Constant, not growing with length: spread across sizes stays tight.
        let min_overhead = *overheads.iter().min().expect("non-empty");
        assert!(
            max_overhead - min_overhead <= 4,
            "edit-over-send overhead must not scale with length, got {overheads:?}"
        );
    }

    #[cfg(feature = "ecies-randomized")]
    mod private_bodies {
        use super::*;
        use crate::ecies::encrypt_with_symmetric_key;

        const SECRET: [u8; 32] = [7u8; 32];

        #[test]
        fn measure_text_matches_private_body() {
            for text in samples() {
                let content_bytes =
                    crate::room_state::content::TextContentV1::new(text.clone()).encode();
                let (ciphertext, nonce) = encrypt_with_symmetric_key(&SECRET, &content_bytes);
                let body = RoomMessageBody::private(
                    CONTENT_TYPE_TEXT,
                    TEXT_CONTENT_VERSION,
                    ciphertext,
                    nonce,
                    1,
                );
                assert_eq!(
                    RoomMessageBody::measure_text(&text, true),
                    body.content_len(),
                    "text bytes={}",
                    text.len()
                );
            }
        }

        #[test]
        fn measure_reply_matches_private_body() {
            for text in samples() {
                let reply = crate::room_state::content::ReplyContentV1::new(
                    text.clone(),
                    target_id(),
                    "Alice".to_string(),
                    "some preview".to_string(),
                );
                let (ciphertext, nonce) = encrypt_with_symmetric_key(&SECRET, &reply.encode());
                let body = RoomMessageBody::private(
                    CONTENT_TYPE_REPLY,
                    REPLY_CONTENT_VERSION,
                    ciphertext,
                    nonce,
                    1,
                );
                assert_eq!(
                    RoomMessageBody::measure_reply(
                        &text,
                        target_id(),
                        "Alice",
                        "some preview",
                        true
                    ),
                    body.content_len(),
                    "text bytes={}",
                    text.len()
                );
            }
        }

        #[test]
        fn measure_edit_matches_private_body() {
            for text in samples() {
                let action =
                    crate::room_state::content::ActionContentV1::edit(target_id(), text.clone());
                let (ciphertext, nonce) = encrypt_with_symmetric_key(&SECRET, &action.encode());
                let body = RoomMessageBody::private_action(ciphertext, nonce, 1);
                assert_eq!(
                    RoomMessageBody::measure_edit(target_id(), &text, true),
                    body.content_len(),
                    "text bytes={}",
                    text.len()
                );
            }
        }
    }
}
