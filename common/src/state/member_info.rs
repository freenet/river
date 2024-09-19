use crate::state::ChatRoomParametersV1;
use crate::state::ChatRoomStateV1;
use crate::state::member::{MemberId, AuthorizedMember, Member};
use crate::util::{sign_struct, verify_struct};
use ed25519_dalek::{Signature, SigningKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemberInfoV1 {
    pub member_info: HashMap<MemberId, AuthorizedMemberInfo>,
}

impl Default for MemberInfoV1 {
    fn default() -> Self {
        MemberInfoV1 {
            member_info: HashMap::new(),
        }
    }
}

impl ComposableState for MemberInfoV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = Vec<MemberId>;
    type Delta = Vec<AuthorizedMemberInfo>;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        for (member_id, member_info) in &self.member_info {
            // Check if the member exists in the parent state
            if !parent_state.members.members_by_member_id().contains_key(member_id) {
                return Err(format!("MemberInfo exists for non-existent member: {:?}", member_id));
            }

            // Verify the signature
            member_info.verify_signature(parameters)?;
        }
        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.member_info.keys().cloned().collect()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Self::Delta {
        let old_members: HashSet<_> = old_state_summary.iter().collect();
        self.member_info
            .values()
            .filter(|info| !old_members.contains(&info.member_info.member_id))
            .cloned()
            .collect()
    }

    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Self::Delta,
    ) -> Result<(), String> {
        for member_info in delta {
            if parent_state
                .members
                .members_by_member_id()
                .contains_key(&member_info.member_info.member_id)
            {
                member_info.verify_signature(parameters)?;
                self.member_info
                    .insert(member_info.member_info.member_id, member_info.clone());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedMemberInfo {
    pub member_info: MemberInfo,
    pub signature: Signature,
}

impl AuthorizedMemberInfo {
    pub fn new(member_info: MemberInfo, signing_key: &SigningKey) -> Self {
        let signature = sign_struct(&member_info, signing_key);
        Self {
            member_info,
            signature,
        }
    }

    pub fn verify_signature(&self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        verify_struct(&self.member_info, &self.signature, &parameters.owner)
            .map_err(|e| format!("Invalid signature: {}", e))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub member_id: MemberId,
    pub version: u32,
    pub preferred_nickname: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn create_test_member_info(member_id: MemberId) -> MemberInfo {
        MemberInfo {
            member_id,
            version: 1,
            preferred_nickname: "TestUser".to_string(),
        }
    }

    #[test]
    fn test_member_info_v1_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::new(&owner_verifying_key);

        let member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1
            .member_info
            .insert(member_id, authorized_member_info);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: SigningKey::generate(&mut OsRng).verifying_key(),
                nickname: "TestUser".to_string(),
            },
            signature: Signature::from_bytes(&[0; 64]).expect("Invalid signature bytes"),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        assert!(member_info_v1.verify(&parent_state, &parameters).is_ok());
    }

    #[test]
    fn test_member_info_v1_summarize() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1
            .member_info
            .insert(member_id, authorized_member_info);

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        let summary = member_info_v1.summarize(&parent_state, &parameters);
        assert_eq!(summary.len(), 1);
        assert!(summary.contains(&member_id));
    }

    #[test]
    fn test_member_info_v1_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id1 = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let member_id2 = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());

        let member_info1 = create_test_member_info(member_id1);
        let member_info2 = create_test_member_info(member_id2);

        let authorized_member_info1 = AuthorizedMemberInfo::new(member_info1, &owner_signing_key);
        let authorized_member_info2 = AuthorizedMemberInfo::new(member_info2, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1
            .member_info
            .insert(member_id1, authorized_member_info1);
        member_info_v1
            .member_info
            .insert(member_id2, authorized_member_info2);

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        let old_summary = vec![member_id1];
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);

        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].member_info.member_id, member_id2);
    }

    #[test]
    fn test_member_info_v1_apply_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        let delta = vec![authorized_member_info.clone()];

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: MemberId::new(&owner_signing_key.verifying_key()),
                invited_by: MemberId::new(&owner_signing_key.verifying_key()),
                member_vk: SigningKey::generate(&mut OsRng).verifying_key(),
                nickname: "TestUser".to_string(),
            },
            signature: Signature::from_bytes(&[0; 64]).expect("Invalid signature bytes"),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        assert!(member_info_v1
            .apply_delta(&parent_state, &parameters, &delta)
            .is_ok());
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(
            member_info_v1.member_info.get(&member_id),
            Some(&authorized_member_info)
        );
    }

    #[test]
    fn test_authorized_member_info_new_and_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let member_info = create_test_member_info(member_id);

        let authorized_member_info = AuthorizedMemberInfo::new(member_info.clone(), &owner_signing_key);

        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        assert!(authorized_member_info.verify_signature(&parameters).is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        let wrong_parameters = ChatRoomParametersV1 { owner: wrong_key };
        assert!(authorized_member_info.verify_signature(&wrong_parameters).is_err());
    }
}
