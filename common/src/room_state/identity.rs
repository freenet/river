use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::member::AuthorizedMember;
use super::member_info::AuthorizedMemberInfo;

const ARMOR_BEGIN: &str = "-----BEGIN RIVER IDENTITY-----";
const ARMOR_END: &str = "-----END RIVER IDENTITY-----";
const LINE_WIDTH: usize = 64;

/// A portable identity bundle containing everything needed to restore
/// a user's room identity on a different client.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IdentityExport {
    /// The room owner's verifying key (identifies which room)
    pub room_owner: VerifyingKey,
    /// The user's private signing key
    pub signing_key: SigningKey,
    /// The user's signed membership proof
    pub authorized_member: AuthorizedMember,
    /// Chain of AuthorizedMembers from this member up to the owner,
    /// needed for membership validation and rejoin after pruning
    pub invite_chain: Vec<AuthorizedMember>,
    /// Optional member info (nickname etc.)
    pub member_info: Option<AuthorizedMemberInfo>,
}

impl IdentityExport {
    /// Encode as an armored string with header/footer and line wrapping.
    pub fn to_armored_string(&self) -> String {
        let mut data = Vec::new();
        ciborium::ser::into_writer(self, &mut data).expect("Serialization should not fail");
        let encoded = bs58::encode(data).into_string();

        let mut result = String::new();
        result.push_str(ARMOR_BEGIN);
        result.push('\n');
        for chunk in encoded.as_bytes().chunks(LINE_WIDTH) {
            result.push_str(std::str::from_utf8(chunk).unwrap());
            result.push('\n');
        }
        result.push_str(ARMOR_END);
        result
    }

    /// Decode from an armored string, stripping header/footer and whitespace.
    pub fn from_armored_string(s: &str) -> Result<Self, String> {
        // Strip armor markers and whitespace
        let payload: String = s
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with("-----"))
            .collect();

        if payload.is_empty() {
            return Err("Empty identity token".to_string());
        }

        let decoded = bs58::decode(&payload)
            .into_vec()
            .map_err(|e| format!("Base58 decode error: {}", e))?;
        let export: Self = ciborium::de::from_reader(&decoded[..])
            .map_err(|e| format!("Deserialization error: {}", e))?;

        // Validate that the signing key matches the authorized member's verifying key
        if export.signing_key.verifying_key() != export.authorized_member.member.member_vk {
            return Err(
                "Signing key does not match the authorized member's verifying key".to_string(),
            );
        }

        Ok(export)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::member::{Member, MemberId};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn test_roundtrip_armored() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        let member_sk = SigningKey::generate(&mut OsRng);
        let member_vk = member_sk.verifying_key();

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        };
        let authorized_member = AuthorizedMember::new(member, &owner_sk);

        let export = IdentityExport {
            room_owner: owner_vk,
            signing_key: member_sk,
            authorized_member,
            invite_chain: vec![],
            member_info: None,
        };

        let armored = export.to_armored_string();

        // Verify format
        assert!(armored.starts_with(ARMOR_BEGIN));
        assert!(armored.trim_end().ends_with(ARMOR_END));

        // Verify all lines are within width limit
        for line in armored.lines() {
            if !line.starts_with("-----") {
                assert!(line.len() <= LINE_WIDTH, "Line too long: {}", line.len());
            }
        }

        // Roundtrip
        let decoded = IdentityExport::from_armored_string(&armored).unwrap();
        assert_eq!(decoded.room_owner.as_bytes(), export.room_owner.as_bytes());
        assert_eq!(
            decoded.signing_key.to_bytes(),
            export.signing_key.to_bytes()
        );
        assert_eq!(decoded.authorized_member, export.authorized_member);
        assert_eq!(decoded.invite_chain.len(), 0);
        assert!(decoded.member_info.is_none());
    }

    #[test]
    fn test_rejects_mismatched_key() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        let member_sk = SigningKey::generate(&mut OsRng);
        let wrong_sk = SigningKey::generate(&mut OsRng);
        let member_vk = member_sk.verifying_key();

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        };
        let authorized_member = AuthorizedMember::new(member, &owner_sk);

        // Use the wrong signing key
        let export = IdentityExport {
            room_owner: owner_vk,
            signing_key: wrong_sk,
            authorized_member,
            invite_chain: vec![],
            member_info: None,
        };

        let armored = export.to_armored_string();
        let result = IdentityExport::from_armored_string(&armored);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not match"));
    }

    #[test]
    fn test_handles_whitespace_and_formatting() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        let member_sk = SigningKey::generate(&mut OsRng);
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        };
        let authorized_member = AuthorizedMember::new(member, &owner_sk);

        let export = IdentityExport {
            room_owner: owner_vk,
            signing_key: member_sk,
            authorized_member,
            invite_chain: vec![],
            member_info: None,
        };

        let armored = export.to_armored_string();

        // Add extra whitespace and blank lines (simulating copy-paste issues)
        let messy = format!("\n  {}  \n\n", armored.replace('\n', "\n  "));
        let decoded = IdentityExport::from_armored_string(&messy).unwrap();
        assert_eq!(
            decoded.signing_key.to_bytes(),
            export.signing_key.to_bytes()
        );
    }
}
