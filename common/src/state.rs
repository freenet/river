use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use ed25519_dalek::VerifyingKey;
use crate::{ChatRoomDelta, ChatRoomParameters, ChatRoomSummary};
use configuration::AuthorizedConfiguration;
use upgrade::AuthorizedUpgrade;
use message::AuthorizedMessage;
use crate::ban::AuthorizedUserBan;

pub mod upgrade;
pub mod member;
pub mod message;
pub mod configuration;

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

    pub fn validate(&self, parameters: &ChatRoomParameters) -> bool {
        let _owner_id = parameters.owner_member_id();
        let banned_members: HashSet<MemberId> = self.ban_log.iter().map(|b| b.ban.banned_user.clone()).collect();
        let member_ids: HashSet<MemberId> = self.members.iter().map(|m| m.member.id()).collect();
        let message_authors: HashSet<MemberId> = self.recent_messages.iter().map(|m| m.author.clone()).collect();
        
        let valid_invitations = self.validate_invitation_chain(&parameters.owner);
        
        banned_members.is_disjoint(&member_ids) && 
        banned_members.is_disjoint(&message_authors) &&
        message_authors.is_subset(&member_ids) &&
        valid_invitations
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

    pub fn apply_delta(&mut self, delta: ChatRoomDelta) {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::configuration::Configuration;
    use crate::state::member::Member;
    use ed25519_dalek::{Signature, VerifyingKey};
    use std::time::SystemTime;

    #[test]
    fn test_delta_application_order() {
        // Create a sample initial state
        let initial_state = ChatRoomState {
            configuration: AuthorizedConfiguration {
                configuration: Configuration {
                    configuration_version: 1,
                    name: "Test Room".to_string(),
                    max_recent_messages: 100,
                    max_user_bans: 10,
                },
                signature: Signature::from_bytes(&[0; 64]),
            },
            members: HashSet::new(),
            upgrade: None,
            recent_messages: Vec::new(),
            ban_log: Vec::new(),
        };

        // Create sample deltas
        let delta1 = ChatRoomDelta {
            configuration: Some(AuthorizedConfiguration {
                configuration: Configuration {
                    configuration_version: 2,
                    name: "Updated Room".to_string(),
                    max_recent_messages: 150,
                    max_user_bans: 15,
                },
                signature: Signature::from_bytes(&[1; 64]),
            }),
            members: HashSet::new(),
            upgrade: None,
            recent_messages: Vec::new(),
            ban_log: Vec::new(),
        };

        let delta2 = ChatRoomDelta {
            configuration: None,
            members: {
                let mut set = HashSet::new();
                set.insert(AuthorizedMember {
                    member: Member {
                        public_key: VerifyingKey::from_bytes(&[
                            215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 
                            14, 225, 114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 26
                        ]).unwrap(),
                        nickname: "Alice".to_string(),
                    },
                    invited_by: VerifyingKey::from_bytes(&[
                        215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 
                        14, 225, 114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 27
                    ]).unwrap(),
                    signature: Signature::from_bytes(&[0; 64]),
                });
                set
            },
            upgrade: None,
            recent_messages: Vec::new(),
            ban_log: Vec::new(),
        };

        let delta3 = ChatRoomDelta {
            configuration: None,
            members: HashSet::new(),
            upgrade: None,
            recent_messages: vec![AuthorizedMessage {
                time: SystemTime::now(),
                content: "Hello, world!".to_string(),
                author: MemberId(1),
                signature: Signature::from_bytes(&[5; 64]),
            }],
            ban_log: Vec::new(),
        };

        // Apply deltas in different orders
        let mut state1 = initial_state.clone();
        state1.apply_delta(delta1.clone());
        state1.apply_delta(delta2.clone());
        state1.apply_delta(delta3.clone());

        let mut state2 = initial_state.clone();
        state2.apply_delta(delta2.clone());
        state2.apply_delta(delta3.clone());
        state2.apply_delta(delta1.clone());

        let mut state3 = initial_state.clone();
        state3.apply_delta(delta3.clone());
        state3.apply_delta(delta1.clone());
        state3.apply_delta(delta2.clone());

        // Compare the final states
        assert_eq!(state1, state2);
        assert_eq!(state2, state3);
    }
}
