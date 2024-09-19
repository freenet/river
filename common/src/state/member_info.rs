use crate::state::ChatRoomParametersV1;
use crate::state::ChatRoomStateV1;
use crate::state::member::MemberId;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInfo {
    pub member_id: MemberId,
    pub version: u32,
    pub preferred_nickname: String,
}
