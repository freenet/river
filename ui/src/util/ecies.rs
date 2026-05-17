//! ECIES helpers for River.
//!
//! The implementation lives in `river_core::ecies` so that the chat delegate
//! and the UI use byte-identical constructions for room-secret distribution.
//! This module re-exports the helpers under their historical names so existing
//! UI imports keep working.
#![allow(unused_imports)]

pub use river_core::ecies::{
    decrypt, decrypt_secret_from_member_blob, decrypt_secret_from_member_blob_raw,
    decrypt_with_symmetric_key, encrypt_secret_for_member, encrypt_with_symmetric_key,
    generate_room_secret, seal_bytes, unseal_bytes, unseal_bytes_with_secrets,
};
