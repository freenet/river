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

/// Computed state for message actions (edits, deletes, reactions)
/// This is rebuilt from action messages and not serialized
#[derive(Clone, PartialEq, Debug, Default)]
pub struct MessageActionsState {
    /// Messages that have been edited: message_id -> new content
    pub edited_content: HashMap<MessageId, RoomMessageBody>,
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
        let current_secret_version = parent_state.secrets.current_version;

        // Validate message constraints before adding
        if let Some(delta) = delta {
            for msg in delta {
                match &msg.message.content {
                    RoomMessageBody::Private { secret_version, .. } => {
                        // In private mode, verify secret version matches current
                        if *privacy_mode == PrivacyMode::Private {
                            if *secret_version != current_secret_version {
                                return Err(format!(
                                    "Private message secret version {} does not match current version {}",
                                    secret_version, current_secret_version
                                ));
                            }
                        }

                        // Verify all current members have encrypted blobs for this version
                        let members = parent_state.members.members_by_member_id();
                        if !parent_state.secrets.has_complete_distribution(&members) {
                            return Err(
                                "Cannot accept private messages: incomplete secret distribution"
                                    .to_string(),
                            );
                        }
                    }
                    RoomMessageBody::Public { .. } => {
                        // In private mode, reject public messages
                        if *privacy_mode == PrivacyMode::Private {
                            return Err("Cannot send public messages in private room".to_string());
                        }
                    }
                    // Action messages (Edit, Delete, Reaction, RemoveReaction) are always allowed
                    // Authorization is checked when applying the action
                    RoomMessageBody::Edit { new_content, .. } => {
                        // For edits in private rooms, the new content must be properly encrypted
                        if *privacy_mode == PrivacyMode::Private {
                            if let RoomMessageBody::Private { secret_version, .. } =
                                new_content.as_ref()
                            {
                                if *secret_version != current_secret_version {
                                    return Err(format!(
                                        "Edit's new content secret version {} does not match current version {}",
                                        secret_version, current_secret_version
                                    ));
                                }
                            } else {
                                return Err("Edit's new content must be encrypted in private room"
                                    .to_string());
                            }
                        }
                    }
                    RoomMessageBody::Delete { .. }
                    | RoomMessageBody::Reaction { .. }
                    | RoomMessageBody::RemoveReaction { .. } => {
                        // These actions don't have content constraints
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
    /// Rebuild the computed actions state by scanning all action messages
    pub fn rebuild_actions_state(&mut self) {
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
            match &msg.message.content {
                RoomMessageBody::Edit {
                    target,
                    new_content,
                } => {
                    // Only the original author can edit their message
                    if let Some(&original_author) = message_authors.get(target) {
                        if actor == original_author {
                            // Don't allow editing deleted messages
                            if !self.actions_state.deleted.contains(target) {
                                self.actions_state
                                    .edited_content
                                    .insert(target.clone(), *new_content.clone());
                            }
                        }
                    }
                }
                RoomMessageBody::Delete { target } => {
                    // Only the original author can delete their message
                    if let Some(&original_author) = message_authors.get(target) {
                        if actor == original_author {
                            self.actions_state.deleted.insert(target.clone());
                            // Also remove any edited content for deleted messages
                            self.actions_state.edited_content.remove(target);
                        }
                    }
                }
                RoomMessageBody::Reaction { target, emoji } => {
                    // Anyone can add reactions to non-deleted messages
                    if message_authors.contains_key(target)
                        && !self.actions_state.deleted.contains(target)
                    {
                        let reactions = self
                            .actions_state
                            .reactions
                            .entry(target.clone())
                            .or_default();
                        let reactors = reactions.entry(emoji.clone()).or_default();
                        // Idempotent: only add if not already present
                        if !reactors.contains(&actor) {
                            reactors.push(actor);
                        }
                    }
                }
                RoomMessageBody::RemoveReaction { target, emoji } => {
                    // Users can only remove their own reactions
                    if let Some(reactions) = self.actions_state.reactions.get_mut(target) {
                        if let Some(reactors) = reactions.get_mut(emoji) {
                            reactors.retain(|r| r != &actor);
                            // Clean up empty entries
                            if reactors.is_empty() {
                                reactions.remove(emoji);
                            }
                        }
                        if reactions.is_empty() {
                            self.actions_state.reactions.remove(target);
                        }
                    }
                }
                RoomMessageBody::Public { .. } | RoomMessageBody::Private { .. } => {
                    // Regular messages don't affect computed state
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

    /// Get the effective content for a message (edited content if edited, original otherwise)
    /// Returns a clone since edited content may have a different lifetime than the original
    pub fn effective_content(&self, message: &AuthorizedMessageV1) -> RoomMessageBody {
        let id = message.id();
        self.actions_state
            .edited_content
            .get(&id)
            .cloned()
            .unwrap_or_else(|| message.message.content.clone())
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

/// Message body that can be either public plaintext or private encrypted
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum RoomMessageBody {
    /// Public plaintext message
    Public { plaintext: String },
    /// Private encrypted message
    Private {
        ciphertext: Vec<u8>,
        nonce: [u8; 12],
        secret_version: SecretVersion,
    },
    /// Edit action: replace target message content (author only)
    Edit {
        target: MessageId,
        new_content: Box<RoomMessageBody>,
    },
    /// Delete action: remove target message (author only)
    Delete { target: MessageId },
    /// Reaction action: add emoji reaction to target message
    Reaction { target: MessageId, emoji: String },
    /// Remove reaction action: remove own emoji reaction from target message
    RemoveReaction { target: MessageId, emoji: String },
}

impl RoomMessageBody {
    /// Create a new public message
    pub fn public(plaintext: String) -> Self {
        Self::Public { plaintext }
    }

    /// Create a new private message
    pub fn private(ciphertext: Vec<u8>, nonce: [u8; 12], secret_version: SecretVersion) -> Self {
        Self::Private {
            ciphertext,
            nonce,
            secret_version,
        }
    }

    /// Create an edit action
    pub fn edit(target: MessageId, new_content: RoomMessageBody) -> Self {
        Self::Edit {
            target,
            new_content: Box::new(new_content),
        }
    }

    /// Create a delete action
    pub fn delete(target: MessageId) -> Self {
        Self::Delete { target }
    }

    /// Create a reaction action
    pub fn reaction(target: MessageId, emoji: String) -> Self {
        Self::Reaction { target, emoji }
    }

    /// Create a remove reaction action
    pub fn remove_reaction(target: MessageId, emoji: String) -> Self {
        Self::RemoveReaction { target, emoji }
    }

    /// Check if this is a public message
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public { .. })
    }

    /// Check if this is a private message
    pub fn is_private(&self) -> bool {
        matches!(self, Self::Private { .. })
    }

    /// Check if this is an action message (edit, delete, reaction, etc.)
    pub fn is_action(&self) -> bool {
        matches!(
            self,
            Self::Edit { .. }
                | Self::Delete { .. }
                | Self::Reaction { .. }
                | Self::RemoveReaction { .. }
        )
    }

    /// Get the target message ID if this is an action
    pub fn target_id(&self) -> Option<&MessageId> {
        match self {
            Self::Edit { target, .. }
            | Self::Delete { target }
            | Self::Reaction { target, .. }
            | Self::RemoveReaction { target, .. } => Some(target),
            Self::Public { .. } | Self::Private { .. } => None,
        }
    }

    /// Get the content length for validation
    pub fn content_len(&self) -> usize {
        match self {
            Self::Public { plaintext } => plaintext.len(),
            Self::Private { ciphertext, .. } => ciphertext.len(),
            Self::Edit { new_content, .. } => new_content.content_len(),
            Self::Delete { .. } => 0,
            Self::Reaction { emoji, .. } | Self::RemoveReaction { emoji, .. } => emoji.len(),
        }
    }

    /// Get the secret version (if private)
    pub fn secret_version(&self) -> Option<SecretVersion> {
        match self {
            Self::Public { .. } => None,
            Self::Private { secret_version, .. } => Some(*secret_version),
            Self::Edit { new_content, .. } => new_content.secret_version(),
            Self::Delete { .. } | Self::Reaction { .. } | Self::RemoveReaction { .. } => None,
        }
    }

    /// Get a string representation for display purposes
    /// This is a temporary helper for UI integration during development
    pub fn to_string_lossy(&self) -> String {
        match self {
            Self::Public { plaintext } => plaintext.clone(),
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
            Self::Edit { target, .. } => format!("[Edit of message {}]", target),
            Self::Delete { target } => format!("[Delete of message {}]", target),
            Self::Reaction { target, emoji } => format!("[Reaction {} to {}]", emoji, target),
            Self::RemoveReaction { target, emoji } => {
                format!("[Remove reaction {} from {}]", emoji, target)
            }
        }
    }

    /// Try to get the public plaintext, returns None if private or action
    pub fn as_public_string(&self) -> Option<&str> {
        match self {
            Self::Public { plaintext } => Some(plaintext),
            _ => None,
        }
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

        let message1 = create_test_message(owner_id, author_id);
        let message2 = create_test_message(owner_id, author_id);
        let message3 = create_test_message(owner_id, author_id);

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
            content: RoomMessageBody::edit(
                original_id.clone(),
                RoomMessageBody::public("Edited content".to_string()),
            ),
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
        let effective = messages.effective_content(&auth_original);
        assert_eq!(effective.as_public_string(), Some("Edited content"));

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
            content: RoomMessageBody::edit(
                original_id.clone(),
                RoomMessageBody::public("Hacked content".to_string()),
            ),
        };
        let auth_edit = AuthorizedMessageV1::new(edit_msg, &other_sk);

        let mut messages = MessagesV1 {
            messages: vec![auth_original.clone(), auth_edit],
            ..Default::default()
        };
        messages.rebuild_actions_state();

        // Edit should be ignored - original content preserved
        assert!(!messages.is_edited(&original_id));
        let effective = messages.effective_content(&auth_original);
        assert_eq!(effective.as_public_string(), Some("Original content"));
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
            content: RoomMessageBody::edit(
                original_id.clone(),
                RoomMessageBody::public("Too late!".to_string()),
            ),
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
        assert_eq!(display[0].message.content.as_public_string(), Some("Hello"));
        assert_eq!(display[1].message.content.as_public_string(), Some("World"));
    }
}
