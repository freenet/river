use crate::state::member::MemberId;
use crate::state::ChatRoomParametersV1;
use crate::state::ChatRoomStateV1;
use crate::util::{sign_struct, verify_struct};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemberInfoV1 {
    pub member_info: Vec<AuthorizedMemberInfo>,
}

impl Default for MemberInfoV1 {
    fn default() -> Self {
        MemberInfoV1 {
            member_info: Vec::new(),
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
        let members_by_id = parent_state.members.members_by_member_id();
        for member_info in &self.member_info {
            // Check if the member exists in the parent state
            let member = members_by_id
                .get(&member_info.member_info.member_id)
                .ok_or_else(|| {
                    format!(
                        "MemberInfo exists for non-existent member: {:?}",
                        member_info.member_info.member_id
                    )
                })?;

            // Verify the signature
            if member.member.member_vk == parameters.owner {
                // If the member is the room owner, verify against the room owner's key
                member_info.verify_signature(parameters)?;
            } else {
                // Otherwise, verify against the member's key
                member_info.verify_signature_with_key(&member.member.member_vk)?;
            }
        }
        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.member_info
            .iter()
            .map(|info| info.member_info.member_id)
            .collect()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let old_members: HashSet<_> = old_state_summary.iter().collect();
        let delta: Vec<AuthorizedMemberInfo> = self
            .member_info
            .iter()
            .filter(|info| !old_members.contains(&info.member_info.member_id))
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
        delta: &Self::Delta,
    ) -> Result<(), String> {
        for member_info in delta {
            let member_id = &member_info.member_info.member_id;
            if let Some(member) = parent_state.members.members_by_member_id().get(member_id) {
                // Verify the signature
                if member.member.member_vk == parameters.owner {
                    // If the member is the room owner, verify against the room owner's key
                    member_info.verify_signature(parameters)?;
                } else {
                    // Otherwise, verify against the member's key
                    member_info.verify_signature_with_key(&member.member.member_vk)?;
                }
                
                // Update or add the member info
                if let Some(existing_info) = self
                    .member_info
                    .iter_mut()
                    .find(|info| info.member_info.member_id == *member_id)
                {
                    if member_info.member_info.version > existing_info.member_info.version {
                        *existing_info = member_info.clone();
                    }
                } else {
                    self.member_info.push(member_info.clone());
                }
            } else {
                return Err(format!("Member {} not found in parent state", member_id));
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
    pub fn new(member_info: MemberInfo, owner_signing_key: &SigningKey) -> Self {
        let signature = sign_struct(&member_info, owner_signing_key);
        Self {
            member_info,
            signature,
        }
    }

    pub fn new_with_member_key(member_info: MemberInfo, member_signing_key: &SigningKey) -> Self {
        let signature = sign_struct(&member_info, member_signing_key);
        Self {
            member_info,
            signature,
        }
    }

    pub fn verify_signature(&self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        self.verify_signature_with_key(&parameters.owner)
    }

    pub fn verify_signature_with_key(&self, verifying_key: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.member_info, &self.signature, verifying_key)
            .map_err(|e| format!("Invalid signature: {}", e))
    }

    // Helper method for tests
    #[cfg(test)]
    pub fn with_invalid_signature(mut self) -> Self {
        self.signature = Signature::from_bytes(&[0; 64]);
        self
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
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use crate::state::member::{AuthorizedMember, Member};

    fn create_test_member_info(member_id: MemberId) -> MemberInfo {
        MemberInfo {
            member_id,
            version: 1,
            preferred_nickname: "TestUser".to_string(),
        }
    }

    #[test]
    fn test_member_info_v1_default() {
        let default_member_info = MemberInfoV1::default();
        assert!(default_member_info.member_info.is_empty());
    }

    #[test]
    fn test_member_info_v1_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::new(&owner_verifying_key);

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = MemberId::new(&member_verifying_key);

        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &member_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1.member_info.push(authorized_member_info.clone());

        let mut parent_state = ChatRoomStateV1::default();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_verifying_key,
        };
        let authorized_member = AuthorizedMember::new(member, &owner_signing_key);
        parent_state.members.members.push(authorized_member);

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = member_info_v1.verify(&parent_state, &parameters);
        assert!(result.is_ok(), "Verification failed: {}", result.unwrap_err());

        // Test with non-existent member
        let non_existent_member_id =
            MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let non_existent_member_info = create_test_member_info(non_existent_member_id);
        let non_existent_authorized_member_info =
            AuthorizedMemberInfo::new(non_existent_member_info, &owner_signing_key);
        member_info_v1
            .member_info
            .push(non_existent_authorized_member_info);

        let verify_result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            verify_result.is_err(),
            "Expected verification to fail, but it succeeded"
        );
        if let Err(err) = verify_result {
            assert!(
                err.contains("MemberInfo exists for non-existent member"),
                "Unexpected error message: {}",
                err
            );
        }

        // Test with invalid signature
        let invalid_authorized_member_info = authorized_member_info.with_invalid_signature();
        member_info_v1.member_info.clear();
        member_info_v1.member_info.push(invalid_authorized_member_info);

        let verify_result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            verify_result.is_err(),
            "Expected verification to fail, but it succeeded"
        );
        if let Err(err) = verify_result {
            assert!(
                err.contains("Invalid signature"),
                "Unexpected error message: {}",
                err
            );
        }
    }

    #[test]
    fn test_member_info_v1_summarize() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1.member_info.push(authorized_member_info);

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
        member_info_v1.member_info.push(authorized_member_info1);
        member_info_v1.member_info.push(authorized_member_info2);

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        let old_summary = vec![member_id1];
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);

        assert!(delta.is_some());
        let delta = delta.unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].member_info.member_id, member_id2);
    }

    #[test]
    fn test_member_info_v1_apply_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::new(&owner_verifying_key);

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = MemberId::new(&member_verifying_key);

        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new_with_member_key(member_info, &member_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        let delta = vec![authorized_member_info.clone()];

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_verifying_key,
            },
            signature: owner_signing_key
                .sign("TestUser".as_bytes())
                .to_bytes()
                .into(),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test applying delta with a new member
        println!("Applying delta with a new member");
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &delta);
        println!("Result: {:?}", result);
        assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(member_info_v1.member_info[0], authorized_member_info);

        // Test applying delta with an existing member (update)
        println!("Applying delta with an existing member (update)");
        let mut updated_member_info = create_test_member_info(member_id);
        updated_member_info.version = 2;
        updated_member_info.preferred_nickname = "UpdatedNickname".to_string();
        let updated_authorized_member_info = AuthorizedMemberInfo::new_with_member_key(updated_member_info, &member_signing_key);
        let update_delta = vec![updated_authorized_member_info.clone()];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &update_delta);
        println!("Result: {:?}", result);
        assert!(result.is_ok(), "Failed to apply update delta: {:?}", result.err());
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(member_info_v1.member_info[0], updated_authorized_member_info);

        // Test applying delta with a non-existent member
        println!("Applying delta with a non-existent member");
        let non_existent_member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let non_existent_member_info = create_test_member_info(non_existent_member_id);
        let non_existent_authorized_member_info = AuthorizedMemberInfo::new_with_member_key(non_existent_member_info, &SigningKey::generate(&mut OsRng));
        let non_existent_delta = vec![non_existent_authorized_member_info];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &non_existent_delta);
        println!("Result: {:?}", result);
        assert!(result.is_err());

        // Test applying delta with an older version (should not update)
        println!("Applying delta with an older version");
        let mut older_member_info = create_test_member_info(member_id);
        older_member_info.version = 1;
        let older_authorized_member_info = AuthorizedMemberInfo::new_with_member_key(older_member_info, &member_signing_key);
        let older_delta = vec![older_authorized_member_info];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &older_delta);
        println!("Result: {:?}", result);
        assert!(result.is_ok(), "Failed to apply older version delta: {:?}", result.err());
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(member_info_v1.member_info[0].member_info.version, 2);

        // Test applying delta with multiple members
        println!("Applying delta with multiple members");
        let new_member_signing_key = SigningKey::generate(&mut OsRng);
        let new_member_verifying_key = new_member_signing_key.verifying_key();
        let new_member_id = MemberId::new(&new_member_verifying_key);
        let new_member_info = create_test_member_info(new_member_id);
        let new_authorized_member_info = AuthorizedMemberInfo::new_with_member_key(new_member_info, &new_member_signing_key);

        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: new_member_verifying_key,
            },
            signature: owner_signing_key
                .sign("NewTestUser".as_bytes())
                .to_bytes()
                .into(),
        });

        let multi_delta = vec![updated_authorized_member_info.clone(), new_authorized_member_info.clone()];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &multi_delta);
        println!("Result: {:?}", result);
        assert!(result.is_ok(), "Failed to apply multi-member delta: {:?}", result.err());
        assert_eq!(member_info_v1.member_info.len(), 2);
        assert!(member_info_v1.member_info.contains(&updated_authorized_member_info));
        assert!(member_info_v1.member_info.contains(&new_authorized_member_info));
    }

    #[test]
    fn test_authorized_member_info_new_and_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
        let member_info = create_test_member_info(member_id);

        let authorized_member_info =
            AuthorizedMemberInfo::new(member_info.clone(), &owner_signing_key);

        let parameters = ChatRoomParametersV1 {
            owner: owner_signing_key.verifying_key(),
        };

        assert!(authorized_member_info.verify_signature(&parameters).is_ok());

        // Test with wrong key
        let wrong_key = SigningKey::generate(&mut OsRng).verifying_key();
        let wrong_parameters = ChatRoomParametersV1 { owner: wrong_key };
        assert!(authorized_member_info
            .verify_signature(&wrong_parameters)
            .is_err());
    }

    #[test]
    fn test_member_info_v1_delta_scenarios() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();

        let mut member_info_v1 = MemberInfoV1::default();
        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Generate 5 member infos
        let member_infos: Vec<AuthorizedMemberInfo> = (0..5)
            .map(|_| {
                let member_id = MemberId::new(&SigningKey::generate(&mut OsRng).verifying_key());
                let member_info = create_test_member_info(member_id);
                AuthorizedMemberInfo::new(member_info, &owner_signing_key)
            })
            .collect();

        // Test when all members are new
        member_info_v1.member_info = member_infos.clone();
        let delta = member_info_v1.delta(&parent_state, &parameters, &vec![]);
        assert_eq!(delta.unwrap().len(), 5);

        // Test when all members are old
        let old_summary: Vec<MemberId> = member_infos
            .iter()
            .map(|info| info.member_info.member_id)
            .collect();
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert!(delta.is_none());

        // Test with a mix of new and old members
        let old_summary = vec![
            member_infos[0].member_info.member_id,
            member_infos[1].member_info.member_id,
        ];
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert_eq!(delta.unwrap().len(), 3);
    }

    #[test]
    fn test_room_owner_member_info() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = MemberId::new(&owner_verifying_key);

        let owner_member_info = create_test_member_info(owner_id);
        let authorized_owner_info = AuthorizedMemberInfo::new(owner_member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1.member_info.push(authorized_owner_info);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner_verifying_key,
            },
            signature: owner_signing_key.sign("TestOwner".as_bytes()).to_bytes().into(),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = member_info_v1.verify(&parent_state, &parameters);
        assert!(result.is_ok(), "Room owner should be allowed to have member info: {:?}", result);
    }
}
