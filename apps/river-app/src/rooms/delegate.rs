//! Talking to River's chat delegate, the store that holds the user's rooms.
//!
//! Reads (`ListRequest`, `GetRequest`), and the web UI's generation compare-and-swap
//! (`GetVersionedRequest`, `CasStoreRequest`), which is the only way this app writes. It never
//! stores without a generation, deletes, or asks the delegate to sign; what it stores, and
//! when, is [`super::work`]'s business.

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

/// The plain read requests this app sends.
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
    message(&msg)
}

/// The compare-and-swap requests, the web UI's `cas_write_delegate_key`
/// (`ui/src/components/app/chat_delegate.rs`): read a key with its generation, then store only
/// if the key is still at the generation read. Nothing else that writes is representable here.
#[derive(Clone, Debug)]
pub enum Versioned {
    Read(Vec<u8>),
    Store {
        key: Vec<u8>,
        value: Vec<u8>,
        generation: u64,
    },
}

pub fn versioned(v: &Versioned) -> ClientRequest<'static> {
    let msg = match v {
        Versioned::Read(key) => ChatDelegateRequestMsg::GetVersionedRequest {
            key: ChatDelegateKey::new(key.clone()),
        },
        Versioned::Store {
            key,
            value,
            generation,
        } => ChatDelegateRequestMsg::CasStoreRequest {
            key: ChatDelegateKey::new(key.clone()),
            value: value.clone(),
            expected_generation: *generation,
        },
    };
    message(&msg)
}

fn message(msg: &ChatDelegateRequestMsg) -> ClientRequest<'static> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(msg, &mut payload).expect("CBOR into a Vec cannot fail");
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

    /// The version of `dep` that `package` resolves to in a Cargo.lock.
    fn resolved(lock: &str, package: &str, dep: &str) -> String {
        let blocks: Vec<&str> = lock.split("[[package]]").collect();
        let name_line = |b: &str| {
            b.lines()
                .find(|l| l.starts_with("name = "))
                .map(str::to_owned)
        };
        let block = blocks
            .iter()
            .find(|b| name_line(b).as_deref() == Some(&format!("name = \"{package}\"")))
            .unwrap_or_else(|| panic!("{package} is in the lockfile"));
        let entry = block
            .lines()
            .map(|l| l.trim().trim_matches(|c| c == '"' || c == ','))
            .find(|l| *l == dep || l.starts_with(&format!("{dep} ")))
            .unwrap_or_else(|| panic!("{package} depends on {dep}"));
        if let Some(version) = entry.strip_prefix(&format!("{dep} ")) {
            return version.to_owned();
        }
        // Only one version is locked, so the entry does not name it.
        let only: Vec<&&str> = blocks
            .iter()
            .filter(|b| name_line(b).as_deref() == Some(&format!("name = \"{dep}\"")))
            .collect();
        assert_eq!(only.len(), 1, "{dep} is locked once");
        only[0]
            .lines()
            .find_map(|l| l.strip_prefix("version = "))
            .expect("a version")
            .trim_matches('"')
            .to_owned()
    }

    /// The client API's wire format is bincode of freenet-stdlib's types, so this app must use
    /// the stdlib the web UI uses: the version river-core resolves to in the root lockfile.
    /// The two workspaces lock separately, so a bump in one alone fails here.
    #[test]
    fn the_wire_format_is_the_web_uis() {
        const ROOT: &str = include_str!("../../../../Cargo.lock");
        const APPS: &str = include_str!("../../../Cargo.lock");
        let root = resolved(ROOT, "river-core", "freenet-stdlib");
        assert_eq!(resolved(APPS, "river-core", "freenet-stdlib"), root);
        assert_eq!(resolved(APPS, "river-app", "freenet-stdlib"), root);
    }
}
