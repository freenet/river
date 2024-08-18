use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use ed25519_dalek::VerifyingKey;
use crate::{ChatRoomDelta, ChatRoomParameters, ChatRoomSummary};
use configuration::AuthorizedConfiguration;
use upgrade::AuthorizedUpgrade;
use message::AuthorizedMessage;
use ban::AuthorizedUserBan;
use crate::state::member::{AuthorizedMember, MemberId};

pub mod upgrade;
pub mod member;
pub mod message;
pub mod configuration;
pub mod ban;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatRoomState {
    pub configuration: AuthorizedConfiguration,
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,
    pub recent_messages: Vec<AuthorizedMessage>,
    pub ban_log: Vec<AuthorizedUserBan>,
}

impl PartialEq for ChatRoomState {
    fn eq(&self, other: &Self) -> bool {
        self.configuration == other.configuration
            && self.upgrade == other.upgrade
            && self.recent_messages == other.recent_messages
            && self.ban_log == other.ban_log
            && {
                let mut self_members: Vec<_> = self.members.iter().collect();
                let mut other_members: Vec<_> = other.members.iter().collect();
                self_members.sort_by_key(|m| m.member.id());
                other_members.sort_by_key(|m| m.member.id());
                self_members == other_members
            }
    }
}

impl Eq for ChatRoomState {}

impl ChatRoomState {
    pub fn summarize(&self) -> ChatRoomSummary {
        ChatRoomSummary {
            configuration_version: self.configuration.configuration.configuration_version,
            member_ids: self.members.iter().map(|m| m.member.id()).collect::<HashSet<_>>(),
            upgrade_version: self.upgrade.as_ref().map(|u| u.upgrade.version),
            recent_message_ids: self.recent_messages.iter().map(|m| m.id()).collect(),
            ban_ids: self.ban_log.iter().map(|b| b.id()).collect(),
        }
    }

    pub fn validate(&self, parameters: &ChatRoomParameters) -> Result<(), String> {
        let banned_members: HashSet<MemberId> = self.ban_log.iter().map(|b| b.ban.banned_user.clone()).collect();
        let member_ids: HashSet<MemberId> = self.members.iter().map(|m| m.member.id()).collect();
        let message_authors: HashSet<MemberId> = self.recent_messages.iter().map(|m| m.author.clone()).collect();
        
        if !banned_members.is_disjoint(&member_ids) {
            return Err(format!("Banned members are still in the room: {:?}", banned_members.intersection(&member_ids).collect::<Vec<_>>()));
        }
        if !banned_members.is_disjoint(&message_authors) {
            return Err(format!("Messages from banned members are still present: {:?}", banned_members.intersection(&message_authors).collect::<Vec<_>>()));
        }
        if !message_authors.is_subset(&member_ids) {
            return Err(format!("Messages from non-members are present: {:?}", message_authors.difference(&member_ids).collect::<Vec<_>>()));
        }
        if !self.validate_invitation_chain(&parameters.owner) {
            return Err("Invalid invitation chain".to_string());
        }
        Ok(())
    }

    fn validate_invitation_chain(&self, owner: &VerifyingKey) -> bool {
        let mut valid_members = HashSet::new();
        valid_members.insert(*owner);

        fn is_valid_member(
            member: &AuthorizedMember,
            valid_members: &mut HashSet<VerifyingKey>,
            members: &HashSet<AuthorizedMember>,
            owner: &VerifyingKey,
        ) -> bool {
            if valid_members.contains(&member.member.public_key) {
                return true;
            }

            if member.invited_by == *owner {
                valid_members.insert(member.member.public_key);
                return true;
            }

            if let Some(inviter) = members.iter().find(|m| m.member.public_key == member.invited_by) {
                if is_valid_member(inviter, valid_members, members, owner) {
                    valid_members.insert(member.member.public_key);
                    return true;
                }
            }

            false
        }

        self.members.iter().all(|member| is_valid_member(member, &mut valid_members, &self.members, owner))
    }
    
    pub fn create_delta(&self, previous_summary: &ChatRoomSummary) -> ChatRoomDelta {
        let new_bans = self.ban_log.iter()
            .filter(|b| !previous_summary.ban_ids.contains(&b.id()))
            .cloned()
            .collect::<Vec<_>>();

        let new_members = self.members.iter()
            .filter(|m| !previous_summary.member_ids.contains(&m.member.id())
                && !new_bans.iter().any(|b| b.ban.banned_user == m.member.id()))
            .cloned()
            .collect::<HashSet<_>>();

        let new_messages = self.recent_messages.iter()
            .filter(|m| !previous_summary.recent_message_ids.contains(&m.id())
                && !new_bans.iter().any(|b| b.ban.banned_user == m.author))
            .cloned()
            .collect::<Vec<_>>();

        ChatRoomDelta {
            configuration: if self.configuration.configuration.configuration_version > previous_summary.configuration_version {
                Some(self.configuration.clone())
            } else {
                None
            },
            members: new_members,
            upgrade: if self.upgrade.as_ref().map(|u| u.upgrade.version) > previous_summary.upgrade_version {
                self.upgrade.clone()
            } else {
                None
            },
            recent_messages: new_messages,
            ban_log: new_bans,
        }
    }

    
    pub fn apply_delta(&mut self, delta: ChatRoomDelta, parameters: &ChatRoomParameters) -> Result<(), String> {
        // Apply configuration
        if let Some(configuration) = delta.configuration {
            if configuration.configuration.configuration_version > self.configuration.configuration.configuration_version {
                self.configuration = configuration;
            }
        }
        
        // Apply upgrade
        if let Some(upgrade) = delta.upgrade {
            self.upgrade = Some(upgrade);
        }

        // Update ban log
        let mut new_bans = self.ban_log.clone();
        new_bans.extend(delta.ban_log);
        new_bans.sort_by_key(|b| (b.ban.banned_at, b.ban.banned_user));
        new_bans.dedup_by_key(|b| (b.ban.banned_at, b.ban.banned_user));
        self.ban_log = new_bans.into_iter()
            .take(self.configuration.configuration.max_user_bans as usize)
            .collect();

        // Update members
        let banned_users: std::collections::HashSet<_> = self.ban_log.iter().map(|b| b.ban.banned_user).collect();
        self.members.retain(|m| !banned_users.contains(&m.member.id()));
        for member in delta.members {
            if !banned_users.contains(&member.member.id()) {
                self.members.insert(member);
            }
        }

        // Update recent messages
        let mut new_messages = self.recent_messages.clone();
        new_messages.extend(delta.recent_messages);
        new_messages.retain(|m| !banned_users.contains(&m.author));
        new_messages.sort_by_key(|m| (m.time, m.id()));
        new_messages.dedup_by_key(|m| m.id());
        self.recent_messages = new_messages.into_iter()
            .take(self.configuration.configuration.max_recent_messages as usize)
            .collect();

        // Sort members to ensure consistent order
        let mut sorted_members: Vec<_> = self.members.drain().collect();
        sorted_members.sort_by_key(|m| m.member.id());
        self.members = sorted_members.into_iter().collect();

        // Validate the state after applying the delta
        match self.validate(parameters) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Invalid state after applying delta: {}. State: {:?}", e, self)),
        }
    }
}

#[cfg(test)]
mod tests;

