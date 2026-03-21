use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::member::{AuthorizedMember, MemberId};
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
    /// Room display name (shown immediately on import before sync completes)
    #[serde(default)]
    pub room_name: Option<String>,
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

        // Validate invite chain signatures where possible.
        // The authorized_member is signed by its inviter. If the inviter is the owner
        // we can verify directly; if it's a chain member, verify against that member's vk.
        export.validate_invite_chain()?;

        Ok(export)
    }

    /// Validate that invite chain signatures are internally consistent.
    /// We verify each member's signature against its inviter's verifying key,
    /// where the inviter is either the room owner or another chain member.
    fn validate_invite_chain(&self) -> Result<(), String> {
        let owner_id = MemberId::from(&self.room_owner);

        // Build a lookup of chain members by MemberId -> VerifyingKey
        let mut vk_by_id: std::collections::HashMap<MemberId, VerifyingKey> =
            std::collections::HashMap::new();
        vk_by_id.insert(owner_id, self.room_owner);
        for chain_member in &self.invite_chain {
            vk_by_id.insert(chain_member.member.id(), chain_member.member.member_vk);
        }

        // Verify the main member's signature
        let inviter_id = self.authorized_member.member.invited_by;
        if let Some(inviter_vk) = vk_by_id.get(&inviter_id) {
            self.authorized_member
                .verify_signature(inviter_vk)
                .map_err(|e| format!("Invalid authorized_member signature: {}", e))?;
        }

        // Verify each chain member's signature
        for chain_member in &self.invite_chain {
            let inviter_id = chain_member.member.invited_by;
            if let Some(inviter_vk) = vk_by_id.get(&inviter_id) {
                chain_member
                    .verify_signature(inviter_vk)
                    .map_err(|e| format!("Invalid invite chain signature: {}", e))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::member::{Member, MemberId};
    use crate::room_state::member_info::MemberInfo;
    use crate::room_state::privacy::SealedBytes;
    use ed25519_dalek::{Signer, SigningKey};
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
            room_name: None,
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
        assert!(decoded.room_name.is_none());
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
            room_name: None,
        };

        let armored = export.to_armored_string();
        let result = IdentityExport::from_armored_string(&armored);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not match"));
    }

    #[test]
    fn test_roundtrip_with_invite_chain_and_member_info() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        // Create a chain: owner -> member_a -> member_b
        let member_a_sk = SigningKey::generate(&mut OsRng);
        let member_a = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_a_sk.verifying_key(),
        };
        let auth_member_a = AuthorizedMember::new(member_a, &owner_sk);

        let member_b_sk = SigningKey::generate(&mut OsRng);
        let member_b = Member {
            owner_member_id: owner_id,
            invited_by: MemberId::from(&member_a_sk.verifying_key()),
            member_vk: member_b_sk.verifying_key(),
        };
        let auth_member_b = AuthorizedMember::new(member_b, &member_a_sk);

        // Create member info with a nickname
        let member_info = MemberInfo {
            member_id: MemberId::from(&member_b_sk.verifying_key()),
            version: 1,
            preferred_nickname: SealedBytes::public("TestUser".as_bytes().to_vec()),
        };
        let auth_member_info = AuthorizedMemberInfo::new_with_member_key(member_info, &member_b_sk);

        let export = IdentityExport {
            room_owner: owner_vk,
            signing_key: member_b_sk.clone(),
            authorized_member: auth_member_b.clone(),
            invite_chain: vec![auth_member_a.clone()],
            member_info: Some(auth_member_info.clone()),
            room_name: Some("Test Room".to_string()),
        };

        let armored = export.to_armored_string();
        let decoded = IdentityExport::from_armored_string(&armored).unwrap();

        // Verify all fields survive roundtrip
        assert_eq!(decoded.invite_chain.len(), 1);
        assert_eq!(decoded.invite_chain[0], auth_member_a);
        assert_eq!(decoded.authorized_member, auth_member_b);
        assert!(decoded.member_info.is_some());
        assert_eq!(
            decoded
                .member_info
                .unwrap()
                .member_info
                .preferred_nickname
                .to_string_lossy(),
            "TestUser"
        );
        assert_eq!(decoded.room_name.as_deref(), Some("Test Room"));
    }

    #[test]
    fn test_imported_key_can_sign() {
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
            room_name: None,
        };

        let armored = export.to_armored_string();
        let decoded = IdentityExport::from_armored_string(&armored).unwrap();

        // Verify the imported key can produce valid signatures
        let message = b"test message";
        let signature = decoded.signing_key.sign(message);
        assert!(decoded
            .authorized_member
            .member
            .member_vk
            .verify_strict(message, &signature)
            .is_ok());
    }

    #[test]
    fn test_rejects_tampered_signature() {
        use ed25519_dalek::Signature;

        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        let member_sk = SigningKey::generate(&mut OsRng);
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        };
        // Create a valid authorized member then tamper with the signature
        let mut bad_auth_member = AuthorizedMember::new(member, &owner_sk);
        // Replace signature with garbage
        bad_auth_member.signature = Signature::from_bytes(&[0u8; 64]);

        let export = IdentityExport {
            room_owner: owner_vk,
            signing_key: member_sk,
            authorized_member: bad_auth_member,
            invite_chain: vec![],
            member_info: None,
            room_name: None,
        };

        let armored = export.to_armored_string();
        let result = IdentityExport::from_armored_string(&armored);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("signature"));
    }

    #[test]
    fn test_rejects_truncated_token() {
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
            room_name: None,
        };

        let armored = export.to_armored_string();

        // Truncate the token in the middle
        let lines: Vec<&str> = armored.lines().collect();
        let truncated = format!(
            "{}\n{}\n{}",
            lines[0],
            &lines[1][..lines[1].len() / 2],
            lines.last().unwrap()
        );
        let result = IdentityExport::from_armored_string(&truncated);
        assert!(result.is_err());
    }

    #[test]
    fn test_rejects_empty_token() {
        let result = IdentityExport::from_armored_string("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty"));

        let result = IdentityExport::from_armored_string(
            "-----BEGIN RIVER IDENTITY-----\n-----END RIVER IDENTITY-----",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty"));
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
            room_name: None,
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

    #[test]
    fn test_backward_compat_no_room_name() {
        // Simulate a token exported from an older version that doesn't include room_name.
        // CBOR map without the room_name field should decode with room_name = None.
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

        // Manually build a CBOR-serializable struct without room_name
        #[derive(Serialize)]
        struct OldExport {
            room_owner: VerifyingKey,
            signing_key: SigningKey,
            authorized_member: AuthorizedMember,
            invite_chain: Vec<AuthorizedMember>,
            member_info: Option<AuthorizedMemberInfo>,
        }
        let old = OldExport {
            room_owner: owner_vk,
            signing_key: member_sk,
            authorized_member,
            invite_chain: vec![],
            member_info: None,
        };
        let mut data = Vec::new();
        ciborium::ser::into_writer(&old, &mut data).unwrap();
        let encoded = bs58::encode(&data).into_string();
        let armored = format!("{}\n{}\n{}", ARMOR_BEGIN, encoded, ARMOR_END);

        let decoded = IdentityExport::from_armored_string(&armored).unwrap();
        assert!(decoded.room_name.is_none());
    }
}
