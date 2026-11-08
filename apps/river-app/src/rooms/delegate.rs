//! Talking to River's chat delegate, the store that holds the user's rooms.
//!
//! Only reads: `ListRequest` and `GetRequest`. This app never stores, deletes or signs.

use std::sync::OnceLock;

use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse};
use freenet_stdlib::prelude::{
    ApplicationMessage, Delegate, DelegateCode, DelegateKey, InboundDelegateMsg,
    OutboundDelegateMsg, Parameters,
};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg};

/// The chat delegate the web UI ships. Its key is a hash of these bytes, so it is the same
/// delegate, and the same stored rooms, as the web UI's. A file read, not a crate dependency:
/// it cannot change the WASM. `apps.yml` runs when it changes.
const CHAT_DELEGATE_WASM: &[u8] =
    include_bytes!("../../../../ui/public/contracts/chat_delegate.wasm");

/// The chat delegate's key, as the web UI computes it (`CHAT_DELEGATE_KEY` in
/// `ui/src/components/app/chat_delegate.rs`): the bundled WASM with empty parameters.
pub fn chat_delegate_key() -> &'static DelegateKey {
    static KEY: OnceLock<DelegateKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let code = DelegateCode::from(CHAT_DELEGATE_WASM.to_vec());
        let params = Parameters::from(Vec::<u8>::new());
        Delegate::from((&code, &params)).key().clone()
    })
}

/// The read requests this app sends. Anything else is unrepresentable here, so this app
/// cannot write delegate storage.
#[derive(Clone, Debug)]
pub enum Read {
    List,
    Get(Vec<u8>),
}

pub fn request(read: &Read) -> ClientRequest<'static> {
    let msg = match read {
        Read::List => ChatDelegateRequestMsg::ListRequest,
        Read::Get(key) => ChatDelegateRequestMsg::GetRequest {
            key: ChatDelegateKey::new(key.clone()),
        },
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&msg, &mut payload).expect("CBOR into a Vec cannot fail");
    ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key: chat_delegate_key().clone(),
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![InboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(payload),
        )],
    })
}

/// The chat delegate's answers in a response, if it is one of the chat delegate's.
pub fn answers(response: &HostResponse) -> Vec<ChatDelegateResponseMsg> {
    let HostResponse::DelegateResponse { key, values } = response else {
        return Vec::new();
    };
    if key != chat_delegate_key() {
        return Vec::new();
    }
    values
        .iter()
        .filter_map(|v| match v {
            OutboundDelegateMsg::ApplicationMessage(m) => {
                ciborium::de::from_reader(m.payload.as_slice()).ok()
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key the web UI uses, which the migration registry and CI record for the committed
    /// WASM: BLAKE3(BLAKE3(wasm) || params) with empty params.
    #[test]
    fn the_delegate_key_is_the_web_uis() {
        let code_hash = blake3::hash(CHAT_DELEGATE_WASM);
        let mut hasher = blake3::Hasher::new();
        hasher.update(code_hash.as_bytes());
        let expected = hasher.finalize();
        assert_eq!(chat_delegate_key().bytes(), expected.as_bytes());
    }

    #[test]
    fn requests_carry_cbor_reads_for_the_chat_delegate() {
        for (read, check) in [
            (Read::List, None),
            (Read::Get(b"room:abc".to_vec()), Some(b"room:abc".to_vec())),
        ] {
            let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
                key,
                inbound,
                ..
            }) = request(&read)
            else {
                panic!("not a delegate request");
            };
            assert_eq!(&key, chat_delegate_key());
            let [InboundDelegateMsg::ApplicationMessage(m)] = inbound.as_slice() else {
                panic!("one application message");
            };
            let msg: ChatDelegateRequestMsg =
                ciborium::de::from_reader(m.payload.as_slice()).unwrap();
            match (msg, check) {
                (ChatDelegateRequestMsg::ListRequest, None) => {}
                (ChatDelegateRequestMsg::GetRequest { key }, Some(k)) => assert_eq!(key.0, k),
                (other, _) => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn answers_are_read_only_from_the_chat_delegate() {
        let msg = ChatDelegateResponseMsg::ListResponse {
            keys: vec![ChatDelegateKey::new(b"rooms_meta".to_vec())],
        };
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&msg, &mut payload).unwrap();
        let values = vec![OutboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(payload),
        )];
        let ours = HostResponse::DelegateResponse {
            key: chat_delegate_key().clone(),
            values: values.clone(),
        };
        assert!(matches!(
            answers(&ours).as_slice(),
            [ChatDelegateResponseMsg::ListResponse { .. }]
        ));
        let other_code = DelegateCode::from(vec![1, 2, 3]);
        let theirs = HostResponse::DelegateResponse {
            key: Delegate::from((&other_code, &Parameters::from(Vec::<u8>::new())))
                .key()
                .clone(),
            values,
        };
        assert!(answers(&theirs).is_empty());
        assert!(answers(&HostResponse::Ok).is_empty());
    }
}
