use crate::util::{sign_struct, truncated_base64, verify_struct};
use blake3::Hash;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Verifier, Signer};
use serde::{Deserialize, Serialize};
use std::fmt;
use freenet_scaffold::ComposableState;
use crate::{ChatRoomState};
use crate::state::ChatRoomParameters;
use crate::state::member::MemberId;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct OptionalUpgrade(pub Option<AuthorizedUpgrade>);

impl Default for OptionalUpgrade {
    fn default() -> Self {
        OptionalUpgrade(None)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthorizedUpgrade {
    pub upgrade: Upgrade,
    pub signature: Signature,
}

impl ComposableState for OptionalUpgrade {
    type ParentState = ChatRoomState;
    type Summary = Option<u8>;
    type Delta = Option<AuthorizedUpgrade>;
    type Parameters = ChatRoomParameters;

    fn verify(&self, _parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        if let Some(upgrade) = &self.0 {
            upgrade.validate(&parameters.owner).map_err(|e| format!("Invalid signature: {}", e))
        } else {
            Ok(())
        }
    }

    fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
        self.0.as_ref().map(|u| u.upgrade.version)
    }

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, _old_state_summary: &Self::Summary) -> Self::Delta {
        self.0.clone()
    }

    fn apply_delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        OptionalUpgrade(delta.clone())
    }
}

impl AuthorizedUpgrade {
    pub fn new(upgrade: Upgrade, signing_key: &SigningKey) -> Self {
        Self {
            upgrade : upgrade.clone(),
            signature : sign_struct(&upgrade, signing_key),
        }
    }

    pub fn validate(&self, verifying_key: &VerifyingKey) -> Result<(), ed25519_dalek::SignatureError> {
        verify_struct(&self.upgrade, &self.signature, &verifying_key)
    }
}

impl fmt::Debug for AuthorizedUpgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedUpgrade")
            .field("upgrade", &self.upgrade)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Upgrade {
    pub owner_member_id: MemberId,
    pub version: u8,
    pub new_chatroom_address: Hash,
}
