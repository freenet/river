//! An invitation to a room, as the web UI encodes it, for clients other than
//! the web UI (the native app).
//!
//! The web UI's type is `Invitation` in `ui/src/components/members.rs`; riverctl
//! has its own copy too. An invite code is `bs58(CBOR(Invitation))`, and an
//! invite link is River's web app URL with `?invitation=<code>`. The drift test
//! in the `ui` crate (`stored_rooms_drift` in `ui/src/room_data.rs`) encodes
//! with each side and decodes with the other, so the two cannot drift apart.
//!
//! Gated on the `invitation` feature (client crates only), so the WASM builds
//! keep byte-identical WASM.

use crate::room_state::member::AuthorizedMember;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// An invitation to the room owned by `room`. It is a bearer credential: it
/// carries the invitee's signing key and, for a private room, the room's
/// secrets, in the clear.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub room: VerifyingKey,
    pub invitee_signing_key: SigningKey,
    /// The invitee's membership, signed by the member who invited them.
    pub invitee: AuthorizedMember,
    /// The room secrets the inviter holds, by version, ascending. Empty for a
    /// public room and in invitations made before the field existed.
    #[serde(default)]
    pub room_secrets: Vec<(u32, [u8; 32])>,
}

impl Invitation {
    /// The invite code: base58 of the CBOR (the web UI's `to_encoded_string`).
    pub fn to_encoded_string(&self) -> Result<String, String> {
        let mut data = Vec::new();
        ciborium::ser::into_writer(self, &mut data).map_err(|e| format!("invitation: {e}"))?;
        Ok(bs58::encode(data).into_string())
    }

    /// Decode an invite code (the web UI's `from_encoded_string`).
    pub fn from_encoded_string(s: &str) -> Result<Self, String> {
        let decoded = bs58::decode(s)
            .into_vec()
            .map_err(|e| format!("Base58 decode error: {e}"))?;
        ciborium::de::from_reader(&decoded[..]).map_err(|e| format!("Deserialization error: {e}"))
    }
}

/// Like the web UI's, this `Debug` hides the room secrets. `SigningKey`'s own
/// `Debug` does not print the key.
impl std::fmt::Debug for Invitation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invitation")
            .field("room", &self.room)
            .field("invitee_signing_key", &self.invitee_signing_key)
            .field("invitee", &self.invitee)
            .field(
                "room_secrets",
                &format_args!("<{} room secret(s) redacted>", self.room_secrets.len()),
            )
            .finish()
    }
}

/// The invite code in what the user pasted: the `invitation` query parameter of
/// an invite link (percent-decoded), else the trimmed text itself. The web UI's
/// `invitation_text_from_scan` (`ui/src/invite_qr.rs`), which its join dialog
/// uses for pasted text.
pub fn invitation_text_from_paste(raw: &str) -> String {
    let trimmed = raw.trim();
    invitation_query_param(trimmed).unwrap_or_else(|| trimmed.to_string())
}

fn invitation_query_param(text: &str) -> Option<String> {
    let query = text.split_once('?')?.1.split('#').next().unwrap_or("");
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "invitation" && !value.is_empty() {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::member::Member;

    fn invitation() -> Invitation {
        let owner = SigningKey::from_bytes(&[1; 32]);
        let invitee = SigningKey::from_bytes(&[2; 32]);
        let member = Member {
            owner_member_id: owner.verifying_key().into(),
            invited_by: owner.verifying_key().into(),
            member_vk: invitee.verifying_key(),
        };
        Invitation {
            room: owner.verifying_key(),
            invitee_signing_key: invitee,
            invitee: AuthorizedMember::new(member, &owner),
            room_secrets: vec![(0, [7; 32])],
        }
    }

    #[test]
    fn a_code_round_trips_and_garbage_is_refused() {
        let inv = invitation();
        let code = inv.to_encoded_string().unwrap();
        assert!(Invitation::from_encoded_string(&code).unwrap() == inv);
        assert!(Invitation::from_encoded_string("0OIl")
            .unwrap_err()
            .starts_with("Base58"));
        assert!(Invitation::from_encoded_string("2g")
            .unwrap_err()
            .starts_with("Deserialization"));
    }

    #[test]
    fn a_pasted_link_yields_its_code() {
        assert_eq!(invitation_text_from_paste("  abc \n"), "abc");
        assert_eq!(
            invitation_text_from_paste("http://127.0.0.1:7509/v1/contract/web/x/?invitation=abc"),
            "abc"
        );
        assert_eq!(
            invitation_text_from_paste("http://h/?a=1&invitation=a%42c#frag"),
            "aBc"
        );
        // An empty parameter is not a code; the text is taken as it is.
        assert_eq!(
            invitation_text_from_paste("http://h/?invitation="),
            "http://h/?invitation="
        );
    }

    #[test]
    fn debug_hides_the_secrets() {
        let shown = format!("{:?}", invitation());
        assert!(shown.contains("redacted"), "{shown}");
        assert!(!shown.contains("[7, 7"), "{shown}");
    }
}
