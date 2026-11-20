//! One room read from the node: the stored copy, its decrypted secrets, and how it turns into
//! the chat screen's view model.
//!
//! This follows the web UI's own reading of a room: the secrets are rebuilt as
//! `RoomData::repopulate_secrets_from_state` does (`ui/src/room_data.rs`), private edits,
//! deletions and reactions as `rebuild_private_actions_state` does, and message bodies decrypt
//! as `try_decrypt_message_content` / `decrypt_message_content` do
//! (`ui/src/components/conversation.rs`).

use std::collections::HashMap;
use std::time::SystemTime;

use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::ContractInstanceId;
use river_core::ecies::{
    decrypt_with_symmetric_key, try_decrypt_secret_from_member_blob_raw, unseal_bytes_with_secrets,
};
use river_core::room_state::configuration::Configuration;
use river_core::room_state::content::{
    CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT, ReplyContentV1, TextContentV1,
};
use river_core::room_state::member::MemberId as RiverMemberId;
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId as RiverMessageId, RoomMessageBody,
};
use river_core::room_state::privacy::{PrivacyMode, SealedBytes};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use river_core::stored_rooms::StoredRoom;

use super::deputies;
use crate::chat::model::{
    self, Appointer, Details, Editable, Limits, Member, MemberId, Message, Reaction, ReplyTo, Role,
    Room, RotateOffer, Usage,
};

/// Why the owner can't change the room's details here (the web UI's `edit-room-no-key-notice`).
pub const NO_KEY_TO_EDIT: &str =
    "This device doesn't hold your key for this room, so you can't change its settings here.";
/// Why the owner of a private room can't change its name or description yet (the web UI's
/// privacy guard, freenet/river#299, which refuses with only a log line).
pub const NO_SECRET_TO_EDIT: &str = "This private room's key hasn't reached this computer yet, so you can't change its settings here until it does.";

/// How many characters of the replied-to message a reply quotes.
const REPLY_PREVIEW_CHARS: usize = 100;

#[derive(Clone)]
pub struct LiveRoom {
    pub stored: StoredRoom,
    /// Decrypted room secrets by version. Empty for a public room.
    secrets: HashMap<u32, [u8; 32]>,
}

impl LiveRoom {
    pub fn new(stored: StoredRoom) -> Self {
        let mut room = LiveRoom {
            stored,
            secrets: HashMap::new(),
        };
        room.refresh();
        room
    }

    pub fn key(&self) -> String {
        bs58::encode(self.stored.owner_vk.to_bytes()).into_string()
    }

    pub fn contract_id(&self) -> ContractInstanceId {
        *self.stored.contract_key.id()
    }

    pub fn params(&self) -> ChatRoomParametersV1 {
        ChatRoomParametersV1 {
            owner: self.stored.owner_vk,
        }
    }

    /// The secret messages are sealed under now, as the web UI's `RoomData::get_secret`: the
    /// room's current version, if this member has it. `None` for a public room.
    pub fn current_secret(&self) -> Option<([u8; 32], u32)> {
        let version = self.stored.room_state.secrets.current_version;
        self.secrets.get(&version).map(|s| (*s, version))
    }

    /// The message (not an action) keyed `key` (see [`message_key`]).
    pub fn message_by_key(&self, key: &str) -> Option<&AuthorizedMessageV1> {
        self.stored
            .room_state
            .recent_messages
            .messages
            .iter()
            .filter(|m| !m.message.content.is_action())
            .find(|m| message_key(&m.id()) == key)
    }

    /// A message's text as the conversation shows it: edited, decrypted, mentions as names.
    pub fn shown_text(&self, m: &AuthorizedMessageV1) -> String {
        let raw = self
            .written_text(m)
            .unwrap_or_else(|| self.body_text(&m.message.content));
        self.render(&raw)
    }

    /// Text as written, with mentions as names.
    fn render(&self, raw: &str) -> String {
        let state = &self.stored.room_state;
        let owner = RiverMemberId::from(&self.stored.owner_vk);
        let me = self
            .stored
            .self_verifying_key()
            .map(|vk| RiverMemberId::from(&vk));
        let resolve_mention = |r: &river_core::mention::MemberRef| -> Option<String> {
            std::iter::once(owner)
                .chain(state.members.members.iter().map(|m| m.member.id()))
                .find(|id| r.matches(*id))
                .map(|id| self.nickname(id, me))
        };
        river_core::mention::render_plaintext_transformed(raw, resolve_mention, |name| clean(&name))
    }

    /// Text as written as markdown for display: each mention a link to
    /// [`model::mention_scheme`] carrying the member's current nickname.
    fn render_md(&self, raw: &str) -> String {
        use river_core::mention::{MentionSegment, parse_segments};
        let state = &self.stored.room_state;
        let owner = RiverMemberId::from(&self.stored.owner_vk);
        let me = self
            .stored
            .self_verifying_key()
            .map(|vk| RiverMemberId::from(&vk));
        let mut out = String::with_capacity(raw.len());
        for seg in parse_segments(raw) {
            match seg {
                MentionSegment::Text(t) => out.push_str(&t),
                MentionSegment::Mention(m) => {
                    let id = std::iter::once(owner)
                        .chain(state.members.members.iter().map(|x| x.member.id()))
                        .find(|id| m.member_ref.matches(*id));
                    let name = match id {
                        Some(id) => self.nickname(id, me),
                        None => m.display_name,
                    };
                    let name = clean(&name);
                    let name = if name.is_empty() {
                        "Member".to_owned()
                    } else {
                        name
                    };
                    // A member this room knows carries their id, so a click on the chip can
                    // open their dialog.
                    let kind = match id {
                        Some(id) if Some(id) == me => format!("self:{id}"),
                        Some(id) => format!("member:{id}"),
                        None => "unknown".to_owned(),
                    };
                    out.push_str(&format!(
                        "[@{}]({}:{kind})",
                        escape_markdown(&name),
                        model::mention_scheme()
                    ));
                }
            }
        }
        out
    }

    /// A message's text as written: edited and decrypted, with mention tokens kept, so an edit
    /// of it keeps its mentions. `None` for a private body this computer cannot decrypt, whose
    /// placeholder must never be offered for editing.
    pub fn written_text(&self, m: &AuthorizedMessageV1) -> Option<String> {
        if let Some(edited) = self.stored.room_state.recent_messages.effective_text(m) {
            return Some(edited);
        }
        match &m.message.content {
            RoomMessageBody::Private { .. } => {
                let (content_type, plain) = self.private_plaintext(&m.message.content)?;
                Some(plain_text(content_type, &plain))
            }
            content @ RoomMessageBody::Public { .. } => Some(self.body_text(content)),
        }
    }

    /// What a reply to `m` carries: its id, its author's name and the start of what it says.
    /// Used both to build a reply and to measure a draft, so the two cannot differ.
    pub fn reply_wrap(&self, m: &AuthorizedMessageV1) -> model::ReplyWrap {
        model::ReplyWrap {
            id: m.id(),
            author_name: self.member_name(m.message.author),
            preview: self
                .shown_text(m)
                .chars()
                .take(model::REPLY_WRAP_CHARS)
                .collect(),
        }
    }

    /// A member's name as the conversation shows it.
    pub fn member_name(&self, id: RiverMemberId) -> String {
        let me = self
            .stored
            .self_verifying_key()
            .map(|vk| RiverMemberId::from(&vk));
        self.nickname(id, me)
    }

    /// Every secret this member holds for the room, by version, ascending: what an invitation
    /// to a private room carries (the web UI's `collect_invitation_secrets`).
    pub fn secrets_by_version(&self) -> Vec<(u32, [u8; 32])> {
        let mut all: Vec<(u32, [u8; 32])> = self.secrets.iter().map(|(v, s)| (*v, *s)).collect();
        all.sort_by_key(|(v, _)| *v);
        all
    }

    pub fn is_private(&self) -> bool {
        self.stored
            .room_state
            .configuration
            .configuration
            .privacy_mode
            == PrivacyMode::Private
    }

    /// Merge a whole room state from the network into the stored copy. All or nothing: a
    /// merge that fails partway leaves the copy as it was.
    pub fn merge_state(&mut self, incoming: &ChatRoomStateV1) -> Result<(), String> {
        let params = self.params();
        let mut next = self.stored.room_state.clone();
        next.merge_through_summary(&params, incoming)?;
        self.stored.room_state = next;
        self.refresh();
        Ok(())
    }

    /// Apply a delta from an update notification. All or nothing, like
    /// [`merge_state`](Self::merge_state): the composed `apply_delta` changes fields one by
    /// one and can fail after some have changed.
    pub fn apply_delta(&mut self, delta: ChatRoomStateV1Delta) -> Result<(), String> {
        let params = self.params();
        let mut next = self.stored.room_state.clone();
        // The composed `apply_delta` ignores its parent-state argument (see the web UI's
        // `RoomSynchronizer::apply_delta_inner`), so a default stands in for it.
        next.apply_delta(&ChatRoomStateV1::default(), &params, &Some(delta))?;
        self.stored.room_state = next;
        self.refresh();
        Ok(())
    }

    /// Rebuild what the stored copy does not persist: the secrets, then the edits, deletions
    /// and reactions (which, in a private room, need the secrets).
    fn refresh(&mut self) {
        self.repopulate_secrets();
        let decrypted = self.decrypted_private_actions();
        self.stored
            .room_state
            .recent_messages
            .rebuild_actions_state_with_decrypted(&decrypted);
    }

    /// The web UI's `repopulate_secrets_from_state`: the owner-signed blobs sealed to this
    /// member, then the invitation's secrets for any version no blob covers.
    fn repopulate_secrets(&mut self) {
        if !self.is_private() {
            return;
        }
        if let Some(sk) = &self.stored.self_sk {
            let me = RiverMemberId::from(&sk.verifying_key());
            for blob in &self.stored.room_state.secrets.encrypted_secrets {
                let s = &blob.secret;
                if s.member_id != me {
                    continue;
                }
                if let Ok(secret) = try_decrypt_secret_from_member_blob_raw(
                    &s.ciphertext,
                    &s.nonce,
                    &s.sender_ephemeral_public_key,
                    sk,
                ) {
                    // The owner-signed blob wins over an invitation's copy of the version.
                    self.secrets.insert(s.secret_version, secret);
                }
            }
        }
        for (version, secret) in &self.stored.invitation_secrets {
            self.secrets.entry(*version).or_insert(*secret);
        }
    }

    fn decrypted_private_actions(&self) -> HashMap<RiverMessageId, Vec<u8>> {
        self.stored
            .room_state
            .recent_messages
            .messages
            .iter()
            .filter(|m| m.message.content.is_action())
            .filter_map(|m| match &m.message.content {
                RoomMessageBody::Private {
                    ciphertext,
                    nonce,
                    secret_version,
                    ..
                } => {
                    let secret = self.secrets.get(secret_version)?;
                    let plain = decrypt_with_symmetric_key(secret, ciphertext, nonce).ok()?;
                    Some((m.id(), plain))
                }
                RoomMessageBody::Public { .. } => None,
            })
            .collect()
    }

    pub(super) fn unseal(&self, sealed: &SealedBytes) -> Option<String> {
        unseal_bytes_with_secrets(sealed, &self.secrets)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// A message body's text: the decrypted text for a private body, and a placeholder when
    /// it can't be read yet (as the web UI's `decrypt_message_content`).
    fn body_text(&self, content: &RoomMessageBody) -> String {
        match content {
            RoomMessageBody::Public { .. } => match content.decode_content() {
                Some(river_core::room_state::content::DecodedContent::Text(t)) => t.text,
                Some(river_core::room_state::content::DecodedContent::Reply(r)) => r.text,
                _ => content.to_string_lossy(),
            },
            RoomMessageBody::Private { secret_version, .. } => {
                match self.private_plaintext(content) {
                    Some((content_type, plain)) => plain_text(content_type, &plain),
                    None if self.secrets.contains_key(secret_version) => content.to_string_lossy(),
                    None if self.secrets.is_empty() => {
                        // The web UI says "Decrypting messages" here because its delegate
                        // back-fills the secret shortly. This app only gets one if a later
                        // update carries it, so it says what is missing.
                        "[Encrypted message - this device doesn't have the room's secret yet]"
                            .into()
                    }
                    None => format!("[Encrypted message - secret v{secret_version} unavailable]"),
                }
            }
        }
    }

    fn private_plaintext(&self, content: &RoomMessageBody) -> Option<(u32, Vec<u8>)> {
        let RoomMessageBody::Private {
            content_type,
            ciphertext,
            nonce,
            secret_version,
            ..
        } = content
        else {
            return None;
        };
        let secret = self.secrets.get(secret_version)?;
        let plain = decrypt_with_symmetric_key(secret, ciphertext, nonce).ok()?;
        Some((*content_type, plain))
    }

    /// The message a reply answers, if this body is a reply that can be read.
    fn reply_target(&self, content: &RoomMessageBody) -> Option<RiverMessageId> {
        match content {
            RoomMessageBody::Public { .. } => match content.decode_content()? {
                river_core::room_state::content::DecodedContent::Reply(r) => {
                    Some(r.target_message_id)
                }
                _ => None,
            },
            RoomMessageBody::Private { .. } => {
                let (content_type, plain) = self.private_plaintext(content)?;
                (content_type == CONTENT_TYPE_REPLY)
                    .then(|| ReplyContentV1::decode(&plain).ok())
                    .flatten()
                    .map(|r| r.target_message_id)
            }
        }
    }

    pub fn name(&self) -> String {
        let display = &self.stored.room_state.configuration.configuration.display;
        match self.unseal(&display.name).map(|n| clean(&n)) {
            Some(n) if !n.is_empty() => n,
            _ if self.is_private() => "Private room".into(),
            _ => "Unnamed room".into(),
        }
    }

    /// The chat screen's view of this room. `minutes` turns a message's time into the chat's
    /// message time (minutes since 1970-01-01 in the local time zone).
    pub fn view(&self, minutes: &dyn Fn(SystemTime) -> u32) -> Room {
        let state = &self.stored.room_state;
        let owner = RiverMemberId::from(&self.stored.owner_vk);
        let me = self
            .stored
            .self_verifying_key()
            .map(|vk| RiverMemberId::from(&vk));

        let mut ids = Ids::default();
        let mut members = Vec::new();
        // Who is a deputy in this viewer's view. Without a local identity nobody is "you" and
        // no grant is relative to anyone, so none shows (as in the web UI).
        let badges = me
            .map(|me| deputies::badges(&state.members, &state.member_info, owner, me))
            .unwrap_or_default();
        // Who you have made a deputy yourself (your own canonical record), whoever else has.
        let my_deputies: &[RiverMemberId] = me
            .map(|me| state.member_info.deputies_of(me))
            .unwrap_or_default();
        let mut push_member = |id: RiverMemberId, role: Role, key: String, ids: &mut Ids| {
            let badge = badges.get(&id);
            let deputized_by = badge
                .map(|b| {
                    b.deputized_by
                        .iter()
                        .map(|a| match *a {
                            a if a == owner => Appointer::Owner,
                            a if Some(a) == me => Appointer::You,
                            a => Appointer::Member(ids.of(a)),
                        })
                        .collect()
                })
                .unwrap_or_default();
            members.push(Member {
                id: ids.of(id),
                nickname: self.nickname(id, me),
                role: if badge.is_some() && role == Role::Member {
                    Role::Deputy
                } else {
                    role
                },
                uid: id.to_string(),
                invited_by: None,
                key,
                claimed: self.claimed_nickname(id),
                deputized_by,
                can_ban_you: badge.and_then(|b| b.can_ban_viewer),
                my_deputy: my_deputies.contains(&id),
                order: id.0.0,
            });
        };
        let key_of = |vk: &ed25519_dalek::VerifyingKey| bs58::encode(vk.as_bytes()).into_string();
        push_member(owner, Role::Owner, key_of(&self.stored.owner_vk), &mut ids);
        let mut inviters = Vec::new();
        for m in &state.members.members {
            let id = m.member.id();
            if id != owner {
                push_member(id, Role::Member, key_of(&m.member.member_vk), &mut ids);
                inviters.push((ids.of(id), m.member.invited_by));
            }
        }
        // The inviter is looked up, not added: someone no longer a member is not listed.
        let mut index: HashMap<MemberId, usize> = HashMap::new();
        for (i, m) in members.iter().enumerate() {
            index.entry(m.id).or_insert(i);
        }
        for (id, inviter) in inviters {
            let inviter = ids.map.get(&inviter).copied();
            if let Some(&i) = index.get(&id) {
                members[i].invited_by = inviter;
            }
        }

        let shown: Vec<&AuthorizedMessageV1> = state
            .recent_messages
            .display_messages()
            .filter(|m| !m.message.content.is_event())
            .collect();
        let message_ids: HashMap<RiverMessageId, model::MessageId> = shown
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id(), i as model::MessageId + 1))
            .collect();
        // Each body decrypted once: as written (for editing), then as shown.
        let written: Vec<Option<String>> = shown.iter().map(|m| self.written_text(m)).collect();
        let texts: Vec<String> = shown
            .iter()
            .zip(&written)
            .map(|(m, w)| match w {
                Some(w) => self.render(w),
                None => self.render(&self.body_text(&m.message.content)),
            })
            .collect();

        let mut messages = Vec::with_capacity(shown.len());
        for (i, m) in shown.iter().enumerate() {
            let id = i as model::MessageId + 1;
            let reply_to = self.reply_target(&m.message.content).and_then(|target| {
                let target_id = *message_ids.get(&target)?;
                let t = shown[target_id as usize - 1];
                Some(ReplyTo {
                    message: target_id,
                    author: ids.of(t.message.author),
                    preview: model::preview(&texts[target_id as usize - 1], REPLY_PREVIEW_CHARS),
                })
            });
            let mut reactions: Vec<Reaction> = state
                .recent_messages
                .reactions(&m.id())
                .into_iter()
                .flatten()
                .map(|(emoji, by)| Reaction {
                    emoji: emoji.clone(),
                    by: by.iter().map(|r| ids.of(*r)).collect(),
                })
                .filter(|r| !r.by.is_empty())
                .collect();
            // The stored reactions are a hash map; give them a stable order.
            reactions.sort_by(|a, b| b.by.len().cmp(&a.by.len()).then(a.emoji.cmp(&b.emoji)));
            messages.push(Message {
                id,
                key: message_key(&m.id()),
                author: ids.of(m.message.author),
                time: minutes(m.message.time),
                text: texts[i].clone(),
                source: written[i].clone().unwrap_or_default(),
                md: written[i]
                    .as_deref()
                    .map(|w| self.render_md(w))
                    .unwrap_or_default(),
                reply_to,
                reactions,
                edited: state.recent_messages.is_edited(&m.id()),
                pending: None,
            });
        }

        // Someone who reacted or wrote but is no longer a member still needs a name.
        for (river_id, id) in ids.extra(members.iter().map(|m| m.id)) {
            members.push(Member {
                uid: river_id.to_string(),
                claimed: false,
                order: river_id.0.0,
                ..Member::new(id, self.nickname(river_id, me), Role::Member)
            });
        }

        let me_id = match me {
            Some(me) => ids.of(me),
            // No local identity: nobody on screen is "you".
            None => 0,
        };
        let unread = self.unread(&shown, me);
        let description = state
            .configuration
            .configuration
            .display
            .description
            .as_ref()
            .and_then(|d| self.unseal(d))
            .map(|d| clean_multiline(&d))
            .unwrap_or_default();
        Room {
            key: self.key(),
            name: self.name(),
            description,
            private: self.is_private(),
            members,
            messages,
            me: me_id,
            unread,
            send_block: None,
            notices: Vec::new(),
            nickname_limit: state
                .configuration
                .configuration
                .max_nickname_size
                .min(river_core::room_state::member_info::NICKNAME_CEILING),
            own_nickname: None,
            nickname_block: None,
            deputy_changes: Vec::new(),
            deputy_block: None,
            deputy_grant_block: None,
            ban_offers: super::bans::offers(self),
            ban_preview: Default::default(),
            deputies_full: my_deputies.len() >= river_core::room_state::member_info::MAX_DEPUTIES,
            details: self.details(),
            wraps: Some(
                shown
                    .iter()
                    .map(|m| (message_key(&m.id()), self.reply_wrap(m)))
                    .collect(),
            ),
        }
    }

    /// What the room details dialog shows beyond the name, description and privacy: the web
    /// UI's `EditRoomModal` fields, and whether this member may change them (its
    /// `OwnerEditGate`: the owner, holding the private half of the key, since every change is
    /// signed).
    pub fn details(&self) -> Details {
        self.details_of(&self.stored.room_state.configuration.configuration)
    }

    /// [`details`](Self::details) as `config`, a configuration of this room (the room's, or
    /// one waiting to be published), would show.
    pub fn details_of(&self, config: &Configuration) -> Details {
        let is_owner = self.stored.self_verifying_key() == Some(self.stored.owner_vk);
        let editable = if !is_owner {
            Editable::No
        } else if self.stored.self_sk.is_none() {
            Editable::Blocked(NO_KEY_TO_EDIT.into())
        } else if self.is_private() && self.current_secret().is_none() {
            Editable::Blocked(NO_SECRET_TO_EDIT.into())
        } else {
            Editable::Yes
        };
        Details {
            contract_id: self.contract_id().to_string(),
            owner_key: bs58::encode(self.stored.owner_vk.to_bytes()).into_string(),
            limits: Limits {
                max_members: config.max_members,
                max_recent_messages: config.max_recent_messages,
                max_message_size: config.max_message_size,
                max_user_bans: config.max_user_bans,
                max_nickname_size: config.max_nickname_size,
                max_room_name: config.max_room_name,
                max_room_description: config.max_room_description,
            },
            usage: Usage {
                members: self.stored.room_state.members.members.len(),
                bans: self.stored.room_state.bans.0.len(),
                messages: self.stored.room_state.recent_messages.messages.len(),
            },
            secret_version: self.stored.room_state.secrets.current_version,
            name_source: self.unseal(&config.display.name).unwrap_or_default(),
            description_source: config
                .display
                .description
                .as_ref()
                .and_then(|d| self.unseal(d))
                .unwrap_or_default(),
            rotate: match (&editable, self.is_private()) {
                (_, false) | (Editable::No, _) => RotateOffer::No,
                (Editable::Yes, true) => RotateOffer::Allowed,
                (Editable::Blocked(why), true) => RotateOffer::Blocked(why.clone()),
            },
            editable,
            pending: None,
            rotation: None,
        }
    }

    /// Messages from others after the last one this user read, as the web UI counts them.
    fn unread(&self, shown: &[&AuthorizedMessageV1], me: Option<RiverMemberId>) -> usize {
        let Some(last) = &self.stored.last_read_message_id else {
            return 0;
        };
        let Some(pos) = shown.iter().position(|m| m.id() == *last) else {
            return 0;
        };
        shown[pos + 1..]
            .iter()
            .filter(|m| Some(m.message.author) != me)
            .count()
    }

    /// Whether `id` has published a nickname this computer can read and that shows something:
    /// the web UI's `claimed_name`, which never protects a name the member did not choose
    /// ("Member <id>" here is the stand-in for its "Unknown", "UNNAMED" and "[Encrypted: ...]").
    fn claimed_nickname(&self, id: RiverMemberId) -> bool {
        self.stored
            .room_state
            .member_info
            .canonical(id)
            .and_then(|i| self.unseal(&i.member_info.preferred_nickname))
            .is_some_and(|n| !shown_nickname(&n).is_empty())
    }

    pub(super) fn nickname(&self, id: RiverMemberId, me: Option<RiverMemberId>) -> String {
        let state = &self.stored.room_state;
        let sealed = state
            .member_info
            .canonical(id)
            .map(|i| &i.member_info.preferred_nickname);
        let name = match sealed {
            // A nickname that can't be decrypted yet shows as "Member <id>" below, not as
            // the sealed value's "[Encrypted: ...]" placeholder.
            Some(s) => self.unseal(s).unwrap_or_default(),
            None if Some(id) == me => self.stored.self_nickname.clone().unwrap_or_default(),
            None => String::new(),
        };
        let name = shown_nickname(&name);
        if name.is_empty() {
            format!("Member {id}")
        } else {
            name
        }
    }
}

/// A decrypted body's text, as the web UI's `try_decrypt_message_content` reads it.
fn plain_text(content_type: u32, plain: &[u8]) -> String {
    if content_type == CONTENT_TYPE_TEXT
        && let Ok(t) = TextContentV1::decode(plain)
    {
        return t.text;
    }
    if content_type == CONTENT_TYPE_REPLY
        && let Ok(r) = ReplyContentV1::decode(plain)
    {
        return r.text;
    }
    String::from_utf8_lossy(plain).into_owned()
}

/// `s` with every ASCII punctuation character backslash-escaped, so it is literal text in
/// markdown (a name like `a]b(c)` cannot end its own link early).
fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The stable key of a River message on screen: its id (a hash of its signature) in decimal.
pub fn message_key(id: &RiverMessageId) -> String {
    id.0.0.to_string()
}

/// River member ids to the small ids the view models use, assigned in order of first use.
#[derive(Default)]
struct Ids {
    map: HashMap<RiverMemberId, MemberId>,
    order: Vec<RiverMemberId>,
}

impl Ids {
    fn of(&mut self, id: RiverMemberId) -> MemberId {
        if let Some(n) = self.map.get(&id) {
            return *n;
        }
        let n = self.order.len() as MemberId + 1;
        self.map.insert(id, n);
        self.order.push(id);
        n
    }

    /// Ids handed out that are not in `listed`.
    fn extra(&self, listed: impl Iterator<Item = MemberId>) -> Vec<(RiverMemberId, MemberId)> {
        let listed: std::collections::HashSet<MemberId> = listed.collect();
        self.order
            .iter()
            .map(|id| (*id, self.map[id]))
            .filter(|(_, n)| !listed.contains(n))
            .collect()
    }
}

/// Characters that draw nothing or reorder text, which a name could use to look like someone
/// else's. A short list next to the web UI's `sanitize_display_name`
/// (`ui/src/util/display_name.rs`), which also folds confusable characters.
pub(crate) fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}' | '\u{200E}' | '\u{200F}'
            | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{E0000}'..='\u{E007F}')
}

/// A one-line name: hidden characters dropped, whitespace runs collapsed, trimmed.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !is_hidden(*c))
        .collect::<String>()
        .split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_multiline(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || !is_hidden(*c))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A nickname as every River client shows it: the web UI's `sanitize_display_name` (copied
/// verbatim in `chat::visual_fold`), so a name with invisible characters reads the same here as
/// in the browser. Empty when there is nothing to show (nothing, or only whitespace and
/// characters that draw nothing), for the stand-in "Member <id>"; the web UI shows "Unnamed"
/// for a name made only of emoji, and so does this.
fn shown_nickname(raw: &str) -> String {
    use crate::chat::visual_fold::{UNNAMED, sanitize_display_name};
    let shown = sanitize_display_name(raw);
    if shown == UNNAMED && clean(raw).is_empty() {
        String::new()
    } else {
        shown
    }
}

/// A time as the chat's message time: minutes since 1970-01-01 in the local time zone.
///
/// A message's time is whatever its author put there, so it can be anywhere a `SystemTime`
/// can be. Never panics: times before 1970 show as 1970, and times past what the chat's
/// message time can hold (the year 10136) show as the last minute it can.
/// (`chrono`'s `From<SystemTime>` panics outside its own range, so it is not used.)
pub fn local_minutes(t: SystemTime) -> u32 {
    use chrono::{Offset, TimeZone};
    let secs: i64 = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => return 0,
    };
    let max_secs = i64::from(u32::MAX) * 60;
    let secs = secs.min(max_secs);
    let offset = chrono::DateTime::from_timestamp(secs, 0)
        .map(|utc| {
            chrono::Local
                .offset_from_utc_datetime(&utc.naive_utc())
                .fix()
                .local_minus_utc()
        })
        .unwrap_or(0);
    (secs + i64::from(offset))
        .div_euclid(60)
        .clamp(0, i64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod md_tests {
    use super::escape_markdown;

    #[test]
    fn a_hostile_name_is_literal_markdown() {
        let name = r"a](file:///x) [b\ <i>&*_`";
        let md = format!("[@{}](dest:member)", escape_markdown(name));
        // Parsed as markdown, the only link is the one we built, and the name is its text.
        let mut dests = Vec::new();
        let mut text = String::new();
        for ev in pulldown_cmark::Parser::new(&md) {
            match ev {
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { dest_url, .. }) => {
                    dests.push(dest_url.to_string())
                }
                pulldown_cmark::Event::Text(t) => text.push_str(&t),
                pulldown_cmark::Event::Html(_) | pulldown_cmark::Event::InlineHtml(_) => {
                    panic!("html")
                }
                _ => {}
            }
        }
        assert_eq!(dests, ["dest:member"]);
        assert_eq!(text, format!("@{name}"));
    }
}
