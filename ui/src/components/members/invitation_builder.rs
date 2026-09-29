//! Mints a fresh, signed invitation to a room, for the invite modal and the invite-via-DM picker.
use crate::components::members::{collect_invitation_secrets, Invitation};
use crate::room_data::RoomData;
use ed25519_dalek::SigningKey;
use river_core::room_state::member::{AuthorizedMember, Member};

pub(crate) async fn create_invitation(room: &RoomData) -> Result<Invitation, String> {
    let Some(inviter) = room.signing_key() else {
        return Err(
            "The local signing key for this room is unavailable, so an invitation cannot be created."
                .into(),
        );
    };
    let invitee_signing_key = SigningKey::generate(&mut rand::thread_rng());
    let member = Member {
        owner_member_id: room.owner_vk.into(),
        invited_by: inviter.verifying_key().into(),
        member_vk: invitee_signing_key.verifying_key(),
    };
    let mut member_bytes = Vec::new();
    ciborium::ser::into_writer(&member, &mut member_bytes)
        .map_err(|_| "Couldn't serialize membership claim. Try again.".to_string())?;
    let signature =
        crate::signing::sign_member_with_fallback(room.room_key(), member_bytes, inviter).await;
    // A private room carries the inviter's secrets so the invitee can read on join.
    let room_secrets = if room.is_private() {
        collect_invitation_secrets(&room.secrets)
    } else {
        Vec::new()
    };
    Ok(Invitation {
        room: room.owner_vk,
        invitee_signing_key,
        invitee: AuthorizedMember::with_signature(member, signature),
        room_secrets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_data::test_minimal_room_data;
    use dioxus::prelude::*;
    use river_core::room_state::member::MemberId;
    use river_core::room_state::privacy::PrivacyMode;

    // Natively the delegate send fails at once, so the room's own key signs (the same bytes).
    fn mint(room: &RoomData) -> Result<Invitation, String> {
        let dom = VirtualDom::new(|| rsx! {});
        dom.in_scope(ScopeId::ROOT, || {
            futures::executor::block_on(create_invitation(room))
        })
    }

    fn room(inviter: &SigningKey, private: bool) -> RoomData {
        let mut room = test_minimal_room_data(SigningKey::from_bytes(&[3; 32]).verifying_key());
        room.self_sk = Some(inviter.clone());
        if private {
            room.room_state.configuration.configuration.privacy_mode = PrivacyMode::Private;
        }
        room
    }

    #[test]
    fn the_invitee_is_signed_by_the_inviter_for_the_candidate_room() {
        let inviter = SigningKey::from_bytes(&[7; 32]);
        let mut candidate = room(&inviter, false);
        candidate.secrets.insert(0, [1; 32]); // a stray secret in a public room must not travel
        let inv = mint(&candidate).expect("a room with a key mints");
        assert_eq!(inv.room, candidate.owner_vk);
        assert_eq!(
            inv.invitee.member.owner_member_id,
            MemberId::from(candidate.owner_vk)
        );
        assert_eq!(
            inv.invitee.member.invited_by,
            MemberId::from(inviter.verifying_key())
        );
        assert_eq!(
            inv.invitee.member.member_vk,
            inv.invitee_signing_key.verifying_key()
        );
        inv.invitee
            .verify_signature(&inviter.verifying_key())
            .expect("the contract accepts it");
        assert!(inv.room_secrets.is_empty());
    }

    #[test]
    fn a_private_room_carries_its_secrets() {
        let inviter = SigningKey::from_bytes(&[7; 32]);
        let mut candidate = room(&inviter, true);
        candidate
            .secrets
            .extend([(9, [9; 32]), (0, [1; 32]), (4, [4; 32])]);
        let inv = mint(&candidate).unwrap();
        assert_eq!(
            inv.room_secrets,
            vec![(0, [1; 32]), (4, [4; 32]), (9, [9; 32])]
        );
    }
}
