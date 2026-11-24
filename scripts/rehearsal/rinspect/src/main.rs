//! Inspector for the migration rehearsal in scripts/rehearsal (room contract
//! and chat delegate re-keys). Run by run.sh; not part of River's workspace.
//!
//!   rinspect state <state.cbor>                         -> JSON fingerprint of a room state
//!   rinspect keys <owner-vk-b58> <room_contract.wasm>   -> current + every legacy contract id
//!   rinspect delegate-key <delegate.wasm>                -> delegate key (b58 + hex), code hash
//!   rinspect get <ws-url> <instance-id-b58> <out>        -> GET raw state; prints the answer SHAPE
//!   rinspect dump <ws-url-with-authToken> <delegate.wasm> <out.json>
//!                                                        -> List + Get every key of a delegate
//!   rinspect params <owner-vk-b58>                      -> room contract parameters (CBOR) on stdout
//!   rinspect members <state.cbor> <owner-vk-b58>        -> members, bans and deputies of a state
//!   rinspect mutual-ban <state.cbor> <owner-vk-b58> <a-rooms.json> <b-rooms.json> <out.cbor>
//!                                                        -> add a ban by each of two members against the other
//!
//! Judged by arithmetic: `state` hashes every element of every collection so
//! two states can be compared as sets, not by eye.

use std::collections::BTreeMap;
use std::time::Duration;

use ciborium::Value;
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, DelegateRequest, HostResponse, WebApi,
};
use freenet_stdlib::prelude::*;
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg};
use river_core::room_state::ChatRoomStateV1;
use serde_json::json;

fn h(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut b = Vec::new();
    ciborium::ser::into_writer(v, &mut b).unwrap();
    b
}

/// Every element of every array reachable one or two levels down, hashed.
fn collect(prefix: &str, v: &Value, out: &mut BTreeMap<String, Vec<String>>) {
    match v {
        Value::Array(items) => {
            let mut hs: Vec<String> = items.iter().map(|i| h(&cbor(i))).collect();
            hs.sort();
            out.insert(prefix.to_string(), hs);
        }
        Value::Map(fields) => {
            for (k, fv) in fields {
                let name = match k {
                    Value::Text(t) => t.clone(),
                    other => format!("{other:?}"),
                };
                if matches!(fv, Value::Array(_) | Value::Map(_)) && prefix.matches('.').count() < 2 {
                    collect(&format!("{prefix}.{name}"), fv, out);
                }
            }
        }
        _ => {}
    }
}

fn state(path: &str) {
    let bytes = std::fs::read(path).expect("read state");
    let s: ChatRoomStateV1 = ciborium::from_reader(bytes.as_slice()).expect("decode ChatRoomStateV1");
    let v: Value = ciborium::from_reader(cbor(&s).as_slice()).unwrap();
    let mut sets = BTreeMap::new();
    collect("state", &v, &mut sets);
    // Arrays of scalars (signatures, byte strings) are noise, not collections.
    sets.retain(|k, _| !k.ends_with(".signature") && !k.contains(".display"));
    let counts: BTreeMap<_, _> = sets.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "bytes": bytes.len(),
            "blake3": blake3::hash(&bytes).to_hex().to_string(),
            "configuration_version": s.configuration.configuration.configuration_version,
            "counts": counts,
            "sets": sets,
        }))
        .unwrap()
    );
}

fn keys(owner_b58: &str, wasm: &str) {
    let vk_bytes: [u8; 32] = bs58::decode(owner_b58).into_vec().unwrap().try_into().unwrap();
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&vk_bytes).unwrap();
    let code_hash: [u8; 32] = *blake3::hash(&std::fs::read(wasm).unwrap()).as_bytes();
    let cur = river_core::migration::contract_key_for_code_hash(&vk, &code_hash);
    println!("current {} {}", hex::encode(&code_hash[..4]), cur.id());
    for ch in river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES.iter().rev() {
        let k = river_core::migration::contract_key_for_code_hash(&vk, ch);
        println!("legacy {} {}", hex::encode(&ch[..4]), k.id());
    }
}

fn delegate_of(wasm: &str) -> (DelegateKey, Parameters<'static>) {
    let code = DelegateCode::from(std::fs::read(wasm).expect("read delegate wasm"));
    let params = Parameters::from(Vec::<u8>::new());
    let d = Delegate::from((&code, &params));
    (d.key().clone(), params)
}

async fn connect(url: &str) -> WebApi {
    let (stream, _) = tokio_tungstenite::connect_async(url).await.expect("connect");
    WebApi::start(stream)
}

async fn get(url: &str, id: &str, out: &str) -> i32 {
    let mut api = connect(url).await;
    let key = ContractInstanceId::from_base58(id).expect("instance id");
    api.send(ClientRequest::ContractOp(ContractRequest::Get {
        key,
        return_contract_code: false,
        subscribe: false,
        blocking_subscribe: false,
    }))
    .await
    .unwrap();
    let t0 = std::time::Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(40), api.recv()).await {
            Err(_) => {
                println!("TIMEOUT after {:.1}s", t0.elapsed().as_secs_f32());
                return 4;
            }
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }))) => {
                std::fs::write(out, state.as_ref()).unwrap();
                println!("STATE {} bytes in {:.1}s", state.as_ref().len(), t0.elapsed().as_secs_f32());
                return 0;
            }
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::NotFound { .. }))) => {
                println!("NOTFOUND (typed) in {:.1}s", t0.elapsed().as_secs_f32());
                return 5;
            }
            Ok(Ok(other)) => {
                println!("OTHER {other:?}");
            }
            Ok(Err(e)) => {
                println!("ERROR after {:.1}s: {e}", t0.elapsed().as_secs_f32());
                return 3;
            }
        }
    }
}

async fn ask(api: &mut WebApi, key: &DelegateKey, params: &Parameters<'static>, msg: &ChatDelegateRequestMsg) -> Result<ChatDelegateResponseMsg, String> {
    api.send(ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key: key.clone(),
        params: params.clone(),
        inbound: vec![InboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(cbor(msg)))],
    }))
    .await
    .map_err(|e| format!("send: {e}"))?;
    loop {
        match tokio::time::timeout(Duration::from_secs(20), api.recv()).await {
            Err(_) => return Err("no answer within 20s".into()),
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                return values
                    .into_iter()
                    .find_map(|v| match v {
                        OutboundDelegateMsg::ApplicationMessage(m) => Some(m.payload),
                        _ => None,
                    })
                    .ok_or_else(|| "an answer holding no message".to_string())
                    .and_then(|p| ciborium::from_reader(p.as_slice()).map_err(|e| format!("decode: {e}")));
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("node error: {e}")),
        }
    }
}

fn show_key(k: &[u8]) -> String {
    match std::str::from_utf8(k) {
        Ok(s) if s.chars().all(|c| !c.is_control()) => s.to_string(),
        _ => format!("hex:{}", hex::encode(k)),
    }
}

async fn dump(url: &str, wasm: &str, out: &str) -> i32 {
    let (key, params) = delegate_of(wasm);
    let mut api = connect(url).await;
    let listed = match ask(&mut api, &key, &params, &ChatDelegateRequestMsg::ListRequest).await {
        Ok(ChatDelegateResponseMsg::ListResponse { keys }) => keys,
        other => {
            println!("LIST {key}: {other:?}");
            std::fs::write(out, serde_json::to_string_pretty(&json!({"delegate": key.encode(), "list_error": format!("{other:?}")})).unwrap()).unwrap();
            return 3;
        }
    };
    let mut entries = BTreeMap::new();
    for k in &listed {
        let r = ask(&mut api, &key, &params, &ChatDelegateRequestMsg::GetRequest { key: k.clone() }).await;
        let e = match r {
            Ok(ChatDelegateResponseMsg::GetResponse { value: Some(v), .. }) => json!({"len": v.len(), "blake3": h(&v), "hex": hex::encode(&v)}),
            Ok(ChatDelegateResponseMsg::GetResponse { value: None, .. }) => json!({"absent": true}),
            other => json!({"error": format!("{other:?}")}),
        };
        entries.insert(show_key(k.as_bytes()), e);
    }
    // The fixed keys the migration probes even when the index is short.
    for fixed in ["rooms_data", "outbound_dms", "rooms_meta"] {
        if !entries.contains_key(fixed) {
            if let Ok(ChatDelegateResponseMsg::GetResponse { value: Some(v), .. }) =
                ask(&mut api, &key, &params, &ChatDelegateRequestMsg::GetRequest { key: ChatDelegateKey(fixed.as_bytes().to_vec()) }).await
            {
                entries.insert(format!("{fixed} (unlisted)"), json!({"len": v.len(), "blake3": h(&v), "hex": hex::encode(&v)}));
            }
        }
    }
    // Signing keys live outside the listed namespace; ask for each room's
    // public key, which proves the delegate can sign as that identity.
    let rooms: Vec<String> = entries.keys().filter_map(|k| k.strip_prefix("room:").map(str::to_string)).collect();
    for r in rooms {
        let Ok(vk) = bs58::decode(&r).into_vec() else { continue };
        let Ok(room_key) = <[u8; 32]>::try_from(vk.as_slice()) else { continue };
        let e = match ask(&mut api, &key, &params, &ChatDelegateRequestMsg::GetPublicKey { room_key }).await {
            Ok(ChatDelegateResponseMsg::GetPublicKeyResponse { public_key: Some(pk), .. }) => json!({"signing_public_key": bs58::encode(pk).into_string()}),
            Ok(ChatDelegateResponseMsg::GetPublicKeyResponse { public_key: None, .. }) => json!({"signing_public_key": null}),
            other => json!({"error": format!("{other:?}")}),
        };
        entries.insert(format!("signing_key_of:{r}"), e);
    }
    println!("DUMP {key}: {} listed keys", listed.len());
    for (k, v) in &entries {
        if let Some(pk) = v.get("signing_public_key") { println!("  {k}: {pk}"); continue; }
        println!("  {k}: {}", v.get("len").map(|l| format!("{l} bytes {}", v["blake3"])).unwrap_or_else(|| v.to_string()));
    }
    std::fs::write(out, serde_json::to_string_pretty(&json!({"delegate": key.encode(), "listed": listed.len(), "entries": entries})).unwrap()).unwrap();
    0
}

fn members(path: &str, owner_b58: &str) {
    use river_core::room_state::ChatRoomParametersV1;
    let bytes = std::fs::read(path).expect("read state");
    let s: ChatRoomStateV1 = ciborium::from_reader(bytes.as_slice()).expect("decode");
    let vk_bytes: [u8; 32] = bs58::decode(owner_b58).into_vec().unwrap().try_into().unwrap();
    let owner = ed25519_dalek::VerifyingKey::from_bytes(&vk_bytes).unwrap();
    let params = ChatRoomParametersV1 { owner };
    let nick = |id: river_core::room_state::member::MemberId| -> String {
        s.member_info.member_info.iter().find(|mi| mi.member_info.member_id == id)
            .map(|mi| match &mi.member_info.preferred_nickname {
                river_core::room_state::privacy::SealedBytes::Public { value } => String::from_utf8_lossy(value).to_string(),
                other => format!("{:?}", other).chars().take(20).collect(),
            })
            .unwrap_or_else(|| "?".into())
    };
    let owner_id = river_core::room_state::member::MemberId::from(&owner);
    let label = |id| if id == owner_id { "owner".to_string() } else { nick(id) };
    // Not `active_members`: that accessor exists only on some branches, and this
    // harness must build against every rehearsed commit.
    let banned = s.members.banned_member_ids(&s.ban_evidence, &s.bans, &s.member_info, &params);
    let active: std::collections::HashSet<_> = s.members.members.iter().map(|m| m.member.id()).filter(|id| !banned.contains(id)).collect();
    for m in &s.members.members {
        let id = m.member.id();
        println!("member {} ({}) invited_by {} active={} enforced_banned={}", id, label(id), label(m.member.invited_by), active.contains(&id), banned.contains(&id));
    }
    for b in &s.bans.0 {
        println!("ban {} bans {} ({} -> {})", b.banned_by, b.ban.banned_user, label(b.banned_by), label(b.ban.banned_user));
    }
    for d in &s.direct_messages.messages {
        let v: Value = ciborium::from_reader(cbor(d).as_slice()).unwrap();
        let _ = v;
    }
    println!("direct_messages {}", s.direct_messages.messages.len());
}

/// Add a ban by each of two members against the other, signed with the keys
/// in their riverctl rooms.json, as two diverged peers would have issued them.
fn mutual_ban(path: &str, owner_b58: &str, a_json: &str, b_json: &str, out: &str) {
    use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
    use river_core::room_state::member::MemberId;
    let bytes = std::fs::read(path).expect("read state");
    let mut s: ChatRoomStateV1 = ciborium::from_reader(bytes.as_slice()).expect("decode");
    let vk_bytes: [u8; 32] = bs58::decode(owner_b58).into_vec().unwrap().try_into().unwrap();
    let owner = ed25519_dalek::VerifyingKey::from_bytes(&vk_bytes).unwrap();
    let key_of = |f: &str| -> ed25519_dalek::SigningKey {
        let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        let b: Vec<u8> = j["rooms"][owner_b58]["signing_key_bytes"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as u8).collect();
        ed25519_dalek::SigningKey::from_bytes(&b.try_into().unwrap())
    };
    let (ka, kb) = (key_of(a_json), key_of(b_json));
    let (ia, ib) = (MemberId::from(&ka.verifying_key()), MemberId::from(&kb.verifying_key()));
    let owner_id = MemberId::from(&owner);
    let now = std::time::SystemTime::now();
    let ban_ab = UserBan { owner_member_id: owner_id, banned_at: now, banned_user: ib };
    let ban_ba = UserBan { owner_member_id: owner_id, banned_at: now, banned_user: ia };
    s.bans.0.push(AuthorizedUserBan::new(ban_ab, ia, &ka));
    s.bans.0.push(AuthorizedUserBan::new(ban_ba, ib, &kb));
    std::fs::write(out, cbor(&s)).unwrap();
    println!("added bans {ia} -> {ib} and {ib} -> {ia}; state now has {} bans", s.bans.0.len());
}






#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let code = match a.get(1).map(String::as_str) {
        Some("state") => {
            state(&a[2]);
            0
        }
        Some("keys") => {
            keys(&a[2], &a[3]);
            0
        }
        Some("delegate-key") => {
            let (k, _) = delegate_of(&a[2]);
            println!("{} {} code_hash={}", k.encode(), hex::encode(k.bytes()), k.code_hash().encode());
            0
        }
        Some("mutual-ban") => {
            mutual_ban(&a[2], &a[3], &a[4], &a[5], &a[6]);
            0
        }
        Some("params") => {
            let vk_bytes: [u8; 32] = bs58::decode(&a[2]).into_vec().unwrap().try_into().unwrap();
            let owner = ed25519_dalek::VerifyingKey::from_bytes(&vk_bytes).unwrap();
            use std::io::Write;
            std::io::stdout().write_all(&cbor(&river_core::room_state::ChatRoomParametersV1 { owner })).unwrap();
            0
        }
        Some("members") => {
            members(&a[2], &a[3]);
            0
        }
        Some("get") => get(&a[2], &a[3], &a[4]).await,
        Some("dump") => dump(&a[2], &a[3], &a[4]).await,
        _ => {
            eprintln!("usage: see source header");
            2
        }
    };
    std::process::exit(code);
}
