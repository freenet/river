use std::collections::HashSet;
use serde::{Serialize, Deserialize};
use ed25519_dalek::VerifyingKey;
use crate::{ChatRoomDelta, ChatRoomParameters, ChatRoomSummary};
use crate::configuration::AuthorizedConfiguration;
use crate::member::{AuthorizedMember, MemberId};
use crate::upgrade::AuthorizedUpgrade;
use crate::message::AuthorizedMessage;
use crate::ban::AuthorizedUserBan;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatRoomState {
    pub configuration: AuthorizedConfiguration,
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,
    pub recent_messages: Vec<AuthorizedMessage>,
    pub ban_log: Vec<AuthorizedUserBan>,
}

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
        let owner_id = parameters.owner_member_id();
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
        if let Some(configuration) = delta.configuration {
            self.configuration = configuration;
        }
        self.members.extend(delta.members);
        if let Some(upgrade) = delta.upgrade {
            self.upgrade = Some(upgrade);
        }
        self.recent_messages.extend(delta.recent_messages);
        self.ban_log.extend(delta.ban_log);
        
        while self.recent_messages.len() > self.configuration.configuration.max_recent_messages as usize {
            let oldest = self.recent_messages.iter().min_by_key(|m| m.time).unwrap().clone();
            self.recent_messages.retain(|m| m.id() != oldest.id());
        }
        while self.ban_log.len() > self.configuration.configuration.max_user_bans as usize {
            let oldest = self.ban_log.iter().min_by_key(|b| b.ban.banned_at).unwrap().clone();
            self.ban_log.retain(|b| b.id() != oldest.id());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::Configuration;
    use crate::member::Member;
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
                signature: Signature::from_bytes(&[0; 64]).expect("Invalid signature"),
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
                signature: Signature::from_bytes(&[1; 64]).expect("Invalid signature"),
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
                    signature: Signature::from_bytes(&[0; 64]).expect("Invalid signature"),
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
                signature: Signature::from_bytes(&[5; 64]).expect("Invalid signature"),
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
