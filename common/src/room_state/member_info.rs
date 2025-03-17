use crate::room_state::member::MemberId;
use crate::room_state::ChatRoomParametersV1;
use crate::room_state::ChatRoomStateV1;
use crate::util::{sign_struct, verify_struct};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct MemberInfoV1 {
    pub member_info: Vec<AuthorizedMemberInfo>,
}

impl ComposableState for MemberInfoV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = HashMap<MemberId, u32>; // Changed from Vec<MemberId> to HashMap<MemberId, u32>
    type Delta = Vec<AuthorizedMemberInfo>;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let members_by_id = parent_state.members.members_by_member_id();
        let owner_id = parameters.owner_id();

        for member_info in &self.member_info {
            let member_id = member_info.member_info.member_id;

            if member_id == owner_id {
                // If this is the owner's member info, verify against owner's key
                member_info.verify_signature(parameters)?;
            } else {
                // For non-owner members, verify they exist in members list
                let member = members_by_id.get(&member_id).ok_or_else(|| {
                    format!("MemberInfo exists for non-existent member: {:?}", member_id)
                })?;

                // Verify the signature with member's key
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
            .map(|info| (info.member_info.member_id, info.member_info.version))
            .collect()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let delta: Vec<AuthorizedMemberInfo> = self
            .member_info
            .iter()
            .filter(|info| {
                // Include if member doesn't exist in old summary OR has a newer version
                !old_state_summary.contains_key(&info.member_info.member_id) || 
                info.member_info.version > *old_state_summary.get(&info.member_info.member_id).unwrap()
            })
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
        if let Some(delta) = delta {
            for member_info in delta {
                let member_id = &member_info.member_info.member_id;
                // Check if this is the room owner
                if *member_id == parameters.owner_id() {
                    // If it's the owner, verify against the room owner's key
                    member_info.verify_signature(parameters)?;
                } else {
                    // For non-owners, verify they exist and check their signature
                    let members = parent_state.members.members_by_member_id();
                    let member = members.get(member_id).ok_or_else(|| {
                        format!("MemberInfo exists for non-existent member: {:?}", member_id)
                    })?;
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
            }
        }
        // Always remove any member info that is not in parent_state.members
        let member_map = parent_state.members.members_by_member_id();
        self.member_info.retain(|info| {
            parameters.owner_id() == info.member_info.member_id
                || member_map.contains_key(&info.member_info.member_id)
        });

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
    use crate::room_state::member::{AuthorizedMember, Member};
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

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
        let owner_id = owner_verifying_key.into();

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();

        let member_info = create_test_member_info(member_id);
        let authorized_member_info = AuthorizedMemberInfo::new(member_info, &member_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1
            .member_info
            .push(authorized_member_info.clone());

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
        assert!(
            result.is_ok(),
            "Verification failed: {}",
            result.unwrap_err()
        );

        // Test with non-existent member
        let non_existent_member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
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
        member_info_v1
            .member_info
            .push(invalid_authorized_member_info);

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
        let member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
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
        assert!(summary.contains_key(&member_id));
        assert_eq!(*summary.get(&member_id).unwrap(), 1); // Version should be 1
    }

    #[test]
    fn test_member_info_v1_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id1 = SigningKey::generate(&mut OsRng).verifying_key().into();
        let member_id2 = SigningKey::generate(&mut OsRng).verifying_key().into();

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

        // Create a HashMap with member_id1 and version 1
        let mut old_summary = HashMap::new();
        old_summary.insert(member_id1, 1);
        
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
        let owner_id = owner_verifying_key.into();

        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();

        let member_info = create_test_member_info(member_id);
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(member_info, &member_signing_key);

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
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(delta));
        println!("Result: {:?}", result);
        assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(member_info_v1.member_info[0], authorized_member_info);

        // Test applying delta with an existing member (update)
        println!("Applying delta with an existing member (update)");
        let mut updated_member_info = create_test_member_info(member_id);
        updated_member_info.version = 2;
        updated_member_info.preferred_nickname = "UpdatedNickname".to_string();
        let updated_authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(updated_member_info, &member_signing_key);
        let update_delta = vec![updated_authorized_member_info.clone()];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(update_delta));
        println!("Result: {:?}", result);
        assert!(
            result.is_ok(),
            "Failed to apply update delta: {:?}",
            result.err()
        );
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(
            member_info_v1.member_info[0],
            updated_authorized_member_info
        );

        // Test applying delta with a non-existent member
        println!("Applying delta with a non-existent member");
        let non_existent_member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
        let non_existent_member_info = create_test_member_info(non_existent_member_id);
        let non_existent_authorized_member_info = AuthorizedMemberInfo::new_with_member_key(
            non_existent_member_info,
            &SigningKey::generate(&mut OsRng),
        );
        let non_existent_delta = vec![non_existent_authorized_member_info];

        let result =
            member_info_v1.apply_delta(&parent_state, &parameters, &Some(non_existent_delta));
        println!("Result: {:?}", result);
        assert!(result.is_err());

        // Test applying delta with an older version (should not update)
        println!("Applying delta with an older version");
        let mut older_member_info = create_test_member_info(member_id);
        older_member_info.version = 1;
        let older_authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(older_member_info, &member_signing_key);
        let older_delta = vec![older_authorized_member_info];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(older_delta));
        println!("Result: {:?}", result);
        assert!(
            result.is_ok(),
            "Failed to apply older version delta: {:?}",
            result.err()
        );
        assert_eq!(member_info_v1.member_info.len(), 1);
        assert_eq!(member_info_v1.member_info[0].member_info.version, 2);

        // Test applying delta with multiple members
        println!("Applying delta with multiple members");
        let new_member_signing_key = SigningKey::generate(&mut OsRng);
        let new_member_verifying_key = new_member_signing_key.verifying_key();
        let new_member_id = new_member_verifying_key.into();
        let new_member_info = create_test_member_info(new_member_id);
        let new_authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(new_member_info, &new_member_signing_key);

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

        let multi_delta = vec![
            updated_authorized_member_info.clone(),
            new_authorized_member_info.clone(),
        ];

        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(multi_delta));
        println!("Result: {:?}", result);
        assert!(
            result.is_ok(),
            "Failed to apply multi-member delta: {:?}",
            result.err()
        );
        assert_eq!(member_info_v1.member_info.len(), 2);
        assert!(member_info_v1
            .member_info
            .contains(&updated_authorized_member_info));
        assert!(member_info_v1
            .member_info
            .contains(&new_authorized_member_info));
    }

    #[test]
    fn test_authorized_member_info_new_and_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
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
                let member_id = SigningKey::generate(&mut OsRng).verifying_key().into();
                let member_info = create_test_member_info(member_id);
                AuthorizedMemberInfo::new(member_info, &owner_signing_key)
            })
            .collect();

        // Test when all members are new
        member_info_v1.member_info = member_infos.clone();
        let delta = member_info_v1.delta(&parent_state, &parameters, &HashMap::new());
        assert_eq!(delta.unwrap().len(), 5);

        // Test when all members are old with same version
        let old_summary: HashMap<MemberId, u32> = member_infos
            .iter()
            .map(|info| (info.member_info.member_id, info.member_info.version))
            .collect();
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert!(delta.is_none());

        // Test with a mix of new and old members
        let mut old_summary = HashMap::new();
        old_summary.insert(member_infos[0].member_info.member_id, 1);
        old_summary.insert(member_infos[1].member_info.member_id, 1);
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert_eq!(delta.unwrap().len(), 3);
        
        // Test with updated version
        let mut updated_member_info = member_infos[0].clone();
        updated_member_info.member_info.version = 2;
        member_info_v1.member_info[0] = updated_member_info;
        
        let delta = member_info_v1.delta(&parent_state, &parameters, &old_summary);
        assert_eq!(delta.unwrap().len(), 4); // 3 new members + 1 updated member
    }

    #[test]
    fn test_member_info_version_handling() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        
        // Create a member
        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();
        
        // Create initial member info with version 1
        let member_info_v1 = create_test_member_info(member_id);
        let authorized_member_info_v1 = 
            AuthorizedMemberInfo::new_with_member_key(member_info_v1, &member_signing_key);
        
        // Create updated member info with version 2
        let mut member_info_v2 = create_test_member_info(member_id);
        member_info_v2.version = 2;
        member_info_v2.preferred_nickname = "UpdatedNickname".to_string();
        let authorized_member_info_v2 = 
            AuthorizedMemberInfo::new_with_member_key(member_info_v2, &member_signing_key);
        
        // Set up state with version 1
        let mut member_info_state = MemberInfoV1::default();
        member_info_state.member_info.push(authorized_member_info_v1.clone());
        
        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };
        
        // Create summary with version 1
        let summary = member_info_state.summarize(&parent_state, &parameters);
        assert_eq!(*summary.get(&member_id).unwrap(), 1);
        
        // Create delta with version 2
        let mut updated_state = MemberInfoV1::default();
        updated_state.member_info.push(authorized_member_info_v2.clone());
        
        let delta = updated_state.delta(&parent_state, &parameters, &summary);
        assert!(delta.is_some());
        assert_eq!(delta.as_ref().unwrap().len(), 1);
        assert_eq!(delta.as_ref().unwrap()[0].member_info.version, 2);
        
        // Apply delta and verify version is updated
        member_info_state.apply_delta(&parent_state, &parameters, &delta).unwrap();
        assert_eq!(member_info_state.member_info.len(), 1);
        assert_eq!(member_info_state.member_info[0].member_info.version, 2);
        assert_eq!(
            member_info_state.member_info[0].member_info.preferred_nickname,
            "UpdatedNickname"
        );
    }

    #[test]
    fn test_room_owner_member_info() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        let owner_member_info = create_test_member_info(owner_id);
        let authorized_owner_info =
            AuthorizedMemberInfo::new(owner_member_info, &owner_signing_key);

        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1.member_info.push(authorized_owner_info);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner_verifying_key,
            },
            signature: owner_signing_key
                .sign("TestOwner".as_bytes())
                .to_bytes()
                .into(),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = member_info_v1.verify(&parent_state, &parameters);
        assert!(
            result.is_ok(),
            "Room owner should be allowed to have member info: {:?}",
            result
        );
    }

    #[test]
    fn test_member_info_retention() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = owner_signing_key.verifying_key();
        let owner_id = owner_verifying_key.into();

        // Create owner's member info
        let owner_member_info = create_test_member_info(owner_id);
        let authorized_owner_info =
            AuthorizedMemberInfo::new(owner_member_info, &owner_signing_key);

        // Create regular member's info
        let member_signing_key = SigningKey::generate(&mut OsRng);
        let member_verifying_key = member_signing_key.verifying_key();
        let member_id = member_verifying_key.into();
        let member_info = create_test_member_info(member_id);
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(member_info, &member_signing_key);

        // Set up MemberInfoV1 with both owner and member info
        let mut member_info_v1 = MemberInfoV1::default();
        member_info_v1
            .member_info
            .push(authorized_owner_info.clone());
        member_info_v1
            .member_info
            .push(authorized_member_info.clone());

        // Set up parent state with only the regular member
        let mut parent_state = ChatRoomStateV1::default();
        parent_state.members.members.push(AuthorizedMember {
            member: Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_verifying_key,
            },
            signature: owner_signing_key
                .sign("TestMember".as_bytes())
                .to_bytes()
                .into(),
        });

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Apply an empty delta to trigger retention logic
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(vec![]));
        assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());

        // Verify that owner's info is retained even though not in members list
        assert!(
            member_info_v1
                .member_info
                .iter()
                .any(|info| info.member_info.member_id == owner_id),
            "Owner's member info should be retained"
        );

        // Remove the regular member from parent state
        parent_state.members.members.clear();

        // Apply another empty delta
        let result = member_info_v1.apply_delta(&parent_state, &parameters, &Some(vec![]));
        assert!(
            result.is_ok(),
            "Failed to apply second delta: {:?}",
            result.err()
        );

        // Verify that only owner's info remains
        assert_eq!(
            member_info_v1.member_info.len(),
            1,
            "Should only contain owner's info"
        );
        assert_eq!(
            member_info_v1.member_info[0].member_info.member_id, owner_id,
            "Remaining info should be owner's"
        );
    }
}
