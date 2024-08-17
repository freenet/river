use std::collections::HashSet;
use std::time::SystemTime;
use blake3::Hash;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use crate::{ChatRoomDelta, ChatRoomSummary};
use crate::util::fast_hash;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatRoomState {
    pub configuration: AuthorizedConfiguration,

    /// Any members excluding any banned members (and their invitees)
    pub members: HashSet<AuthorizedMember>,
    pub upgrade: Option<AuthorizedUpgrade>,

    /// Any authorized messages (which should exclude any messages from banned users)
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

    pub fn create_delta(&self, previous_summary: &ChatRoomSummary) -> ChatRoomDelta {

        // Identify AuthorizedMembers that aren't present in the summary
        let new_bans = self.ban_log.iter().filter(|b| !previous_summary.ban_ids.contains(&b.id()))
            .map (|b| b.clone())
            .collect::<Vec<_>>();

        // Identify new AuthorizedMembers that aren't present in the summary that aren't banned
        let new_members = self.members.iter()
            .filter(|m| !previous_summary.member_ids.contains(&m.member.id())
                && !new_bans.iter().any(|b| b.ban.banned_user == m.member.id()))
            .map (|m| m.clone())
            .collect::<HashSet<_>>();

        // Identify new AuthorizedMessages that aren't present in the summary and aren't from banned users
        let new_messages : Vec<AuthorizedMessage> = self.recent_messages.iter()
            .filter(|m| !previous_summary.recent_message_ids.contains(&m.id())
                && !new_bans.iter().any(|b| b.ban.banned_user == m.author))
            .map (|m| m.clone())
            .collect();

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
        // respect max_recent_messages and max_user_bans, deleting the oldest messages and bans if necessary
        while self.recent_messages.len() > self.configuration.configuration.max_recent_messages as usize {
            // identify the oldest and remove
            let oldest = self.recent_messages.iter().min_by_key(|m| m.time).unwrap().clone();
            self.recent_messages.retain(|m| m.id() != oldest.id());
        }
        while self.ban_log.len() > self.configuration.configuration.max_user_bans as usize {
            // identify the oldest and remove
            let oldest = self.ban_log.iter().min_by_key(|b| b.ban.banned_at).unwrap().clone();
            self.ban_log.retain(|b| b.id() != oldest.id());
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthorizedConfiguration {
    pub configuration: Configuration,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Configuration {
    pub configuration_version: u32,
    pub name: String,
    pub max_recent_messages: u32,
    pub max_user_bans: u32,
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct AuthorizedMember {
    pub member: Member,
    pub invited_by: VerifyingKey,
    pub signature: Signature,
}

// Need Hash for AuthorizedMember to use in HashSet
impl std::hash::Hash for AuthorizedMember {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.member.hash(state);
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Clone, Debug)]
pub struct Member {
    pub public_key: VerifyingKey,
    pub nickname: String,
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug)]
pub struct MemberId(i32);

impl Member {
    pub fn id(&self) -> MemberId {
        // use fasht_hash to hash the public key
        MemberId(fast_hash(&self.public_key.to_bytes()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthorizedUpgrade {
    pub upgrade: Upgrade,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Upgrade {
    pub version: u8,
    pub new_chatroom_address: Hash,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthorizedMessage {
    pub time: SystemTime,
    pub content: String,
    pub author: MemberId,
    pub signature: Signature, // time and content
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug)]
pub struct MessageId(i32);

// TODO: Consider impact of deliberate message id collisions
impl AuthorizedMessage {
    pub fn id(&self) -> MessageId {
        MessageId(fast_hash(&self.signature.to_bytes()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthorizedUserBan {
    pub ban: UserBan,
    pub banned_by: VerifyingKey,
    pub signature: Signature,
}

impl AuthorizedUserBan {
    pub fn id(&self) -> BanId {
        BanId(fast_hash(&self.signature.to_bytes()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserBan {
    pub banned_at: SystemTime,
    pub banned_user: MemberId,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Hash, Debug)]
pub struct BanId(i32);

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;

    // A helper function that takes several ChatRoomDeltas and applies them in every permutation,
    // along with every subset of the deltas, and verifies that the resulting ChatRoomState is the same
    // regardless of the order of application
    fn test_permutations(initial: ChatRoomState, deltas: Vec<ChatRoomDelta>) {
        let n = deltas.len();
        let mut results = std::collections::HashMap::new();

        // Generate all possible subsets of deltas
        for k in 1..=n {
            for subset in deltas.iter().combinations(k) {
                // Generate all permutations of the current subset
                for perm in subset.into_iter().permutations(k) {
                    let mut state = initial.clone();
                    for delta in perm {
                        state.apply_delta(delta.clone());
                    }
                    let summary = state.summarize();
                    results.entry(k).or_insert_with(Vec::new).push(summary);
                }
            }
        }

        // Verify that all results for each subset size are identical
        for (k, summaries) in results {
            assert!(summaries.windows(2).all(|w| w[0] == w[1]),
                    "Inconsistent results for subset size {}", k);
        }
    }

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
                        ]).expect("Invalid public key"),
                        nickname: "Alice".to_string(),
                    },
                    invited_by: VerifyingKey::from_bytes(&[
                        215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 
                        14, 225, 114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 27
                    ]).expect("Invalid public key"),
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
        assert_eq!(state1.summarize(), state2.summarize());
        assert_eq!(state2.summarize(), state3.summarize());
    }
}
