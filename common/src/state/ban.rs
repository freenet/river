use crate::state::member::{AuthorizedMember, MemberId};
use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Verifier, Signer};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use std::collections::HashMap;
use freenet_scaffold::ComposableState;
use freenet_scaffold::util::{fast_hash, FastHash};
use crate::ChatRoomStateV1;
use crate::state::ChatRoomParametersV1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BansV1(pub Vec<AuthorizedUserBan>);

impl Default for BansV1 {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl ComposableState for BansV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = ();
    type Delta = ();
    type Parameters = ChatRoomParametersV1;

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        todo!()
    }

    fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary {
        todo!()
    }

    fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
        todo!()
    }

    fn apply_delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        todo!()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedUserBan {
    pub owner_member_id: i32,
    pub ban: UserBan,
    pub banned_by: VerifyingKey,
    pub signature: Signature,
}

impl AuthorizedUserBan {
    pub fn new(owner_member_id: i32, ban: UserBan, banned_by: VerifyingKey, signing_key: &SigningKey) -> Self {
        let mut data_to_sign = Vec::new();
        data_to_sign.extend_from_slice(&owner_member_id.to_le_bytes());
        ciborium::ser::into_writer(&ban, &mut data_to_sign).expect("Serialization should not fail");
        data_to_sign.extend_from_slice(banned_by.as_bytes());
        let signature = signing_key.sign(&data_to_sign);
        
        Self {
            owner_member_id,
            ban,
            banned_by,
            signature,
        }
    }

    pub fn verify(&self, invitation_chain: &HashMap<VerifyingKey, VerifyingKey>, owner: &VerifyingKey) -> Result<(), String> {
        // First, verify the signature
        let mut data_to_sign = Vec::new();
        data_to_sign.extend_from_slice(&self.owner_member_id.to_le_bytes());
        ciborium::ser::into_writer(&self.ban, &mut data_to_sign).expect("Serialization should not fail");
        data_to_sign.extend_from_slice(self.banned_by.as_bytes());
        
        if self.banned_by.verify(&data_to_sign, &self.signature).is_err() {
            return Err("Invalid ban signature".to_string());
        }

        // Check if the banner is the owner (who can ban anyone)
        if self.banned_by == *owner {
            return Ok(());
        }

        // Find the banned user's public key
        let banned_key = invitation_chain.keys()
            .find(|&&k| fast_hash(&k.to_bytes()) == self.ban.banned_user.0)
            .ok_or("Banned user not found in invitation chain")?;

        // Check if the banner is upstream of the banned user in the invitation chain
        let mut current = *banned_key;
        while let Some(&inviter) = invitation_chain.get(&current) {
            if inviter == self.banned_by {
                return Ok(());
            }
            if inviter == *owner {
                break;
            }
            current = inviter;
        }

        Err("Banning user does not have the authority to ban this member".to_string())
    }

    pub fn id(&self) -> BanId {
        BanId(fast_hash(&self.signature.to_bytes()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserBan {
    pub banned_at: SystemTime,
    pub banned_user: MemberId,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Hash, Debug)]
pub struct BanId(pub FastHash);
