//! The background work behind [`super::Link`]: get a grant, connect, read the stored rooms,
//! show them, then subscribe and keep them current. Runs on one worker thread.

use std::collections::HashMap;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractError, ContractRequest, ContractResponse, DelegateError,
    ErrorKind, HostResponse, RequestError,
};
use freenet_stdlib::prelude::{ContractInstanceId, UpdateData};
use river_core::chat_delegate::{ChatDelegateResponseMsg, LIST_REFUSED_MARKER};
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use river_core::stored_rooms::{
    ROOMS_META_KEY, StoredRoomSlot, StoredRoomsMeta, decode_room_slot, decode_rooms_meta,
    parse_room_storage_key, room_storage_key,
};

use super::delegate::{self, Read};
use super::grant::{GrantOutcome, GrantToken, NodeHttp, request_grant};
use super::room::{LiveRoom, local_minutes};
use super::socket::{NodeSocket, SocketError, TokenVerdict, token_verdict};
use super::token::TokenStore;
pub use crate::chat::LinkStatus;
use crate::chat::Room;

/// How long the delegate gets to answer a read. The web UI allows 10 seconds per request
/// (first-time WASM compilation can be slow); the whole load gets a little more.
const DELEGATE_DEADLINE: Duration = Duration::from_secs(20);

/// How long a subscribed connection may be silent before River checks it is still there.
const IDLE_CHECK: Duration = Duration::from_secs(60);

/// How often the subscribed connection wakes to check the two limits here.
const TICK: Duration = Duration::from_secs(5);

/// How long the room list says it is catching up before it stops waiting for rooms that
/// have not answered their subscribe.
const CATCH_UP_LIMIT: Duration = Duration::from_secs(45);

const BACKOFF_FIRST: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Where getting at the rooms stands, before they can be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Example data with nothing behind it (snapshots, tests).
    Example,
    /// Opening a connection or reading the rooms.
    Connecting,
    /// The user has to allow River on the page at `approve_url`.
    WaitingForApproval { approve_url: String },
    /// The user said no, or did not answer before the request expired.
    NotApproved { expired: bool },
    /// The node is too old to grant River its identity.
    Unsupported,
    /// The node refused for a reason the user has to deal with.
    Refused(String),
    /// No node answered. River keeps trying.
    Unreachable(String),
    /// The rooms are read; the chat screen shows them (or that there are none).
    Ready,
}

impl Access {
    /// Whether the background work has stopped and only the user can restart it.
    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            Access::NotApproved { .. } | Access::Unsupported | Access::Refused(_)
        )
    }
}

/// What the background work reports to the window.
#[derive(Clone, Debug)]
pub enum RoomsUpdate {
    Access(Access),
    /// The rooms, in the room list's order, with the room the web UI had open.
    Rooms {
        rooms: Vec<Room>,
        open: Option<String>,
        status: LinkStatus,
        /// The current time in message time (local minutes since 1970-01-01).
        now: u32,
    },
    /// The connection's state changed while rooms are shown.
    Status(LinkStatus),
}

pub(super) fn run(
    authority: &str,
    config_dir: Option<&Path>,
    report: &dyn Fn(RoomsUpdate),
) -> Access {
    let store = config_dir.map(TokenStore::in_dir);
    let http = NodeHttp::new(&format!("http://{authority}"));
    let mut token: Option<GrantToken> = None;
    let mut rooms = Rooms::default();
    let mut backoff = BACKOFF_FIRST;
    report(RoomsUpdate::Access(Access::Connecting));
    loop {
        let (current, fresh) = match token.clone().or_else(|| store.as_ref()?.load()) {
            Some(t) => (t, false),
            None => {
                let outcome = request_grant(&http, &mut |url| {
                    report(RoomsUpdate::Access(Access::WaitingForApproval {
                        approve_url: url.to_owned(),
                    }))
                });
                match outcome {
                    GrantOutcome::Approved(t) => {
                        if let Some(store) = &store
                            && let Err(e) = store.save(&t)
                        {
                            // Keep using it for this run; the next run asks again.
                            eprintln!("River: {e}");
                        }
                        report(RoomsUpdate::Access(Access::Connecting));
                        (t, true)
                    }
                    GrantOutcome::Denied => return Access::NotApproved { expired: false },
                    GrantOutcome::Expired => return Access::NotApproved { expired: true },
                    GrantOutcome::Unsupported => return Access::Unsupported,
                    GrantOutcome::Refused(m) => return Access::Refused(m),
                    GrantOutcome::Unreachable(e) => {
                        rooms.lost(&e, report);
                        pause(&mut backoff);
                        continue;
                    }
                }
            }
        };
        token = Some(current.clone());
        let err = follow(authority, &current, &mut rooms, report, &mut backoff);
        match token_verdict(&err) {
            TokenVerdict::Discard => {
                token = None;
                if let Some(store) = &store {
                    store.discard();
                }
                if fresh {
                    // A token the node just issued names nothing: asking again would only
                    // open another approval page.
                    return Access::Refused(
                        "Freenet didn't accept the permission it just gave River. Try again."
                            .into(),
                    );
                }
                report(RoomsUpdate::Access(Access::Connecting));
            }
            TokenVerdict::Unusable => {
                return Access::Refused(
                    "Freenet won't let River use its permission over this connection. River \
                     has to reach Freenet directly on this computer, at 127.0.0.1:7509."
                        .into(),
                );
            }
            TokenVerdict::Retry => {
                if let SocketError::Failed(m) = &err
                    && m.starts_with(REFUSED_PREFIX)
                {
                    return Access::Refused(m.clone());
                }
                rooms.lost(&err.to_string(), report);
                pause(&mut backoff);
            }
        }
    }
}

const REFUSED_PREFIX: &str = "River's storage on this computer refused";

fn pause(backoff: &mut Duration) {
    thread::sleep(*backoff);
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
}

/// Connect, read the rooms, then follow them until the connection fails. Returns why.
fn follow(
    authority: &str,
    token: &GrantToken,
    rooms: &mut Rooms,
    report: &dyn Fn(RoomsUpdate),
    backoff: &mut Duration,
) -> SocketError {
    let mut socket = match NodeSocket::connect(authority, token) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(e) = load(&mut socket, rooms) {
        return e;
    }
    *backoff = BACKOFF_FIRST;
    rooms.report(LinkStatus::Loading, report);
    report(RoomsUpdate::Access(Access::Ready));
    for id in rooms.contract_ids() {
        let get = ContractRequest::Get {
            key: id,
            return_contract_code: false,
            subscribe: true,
            blocking_subscribe: false,
        };
        if let Err(e) = socket.send(&ClientRequest::ContractOp(get)) {
            return e;
        }
    }
    if let Err(e) = socket.set_read_timeout(TICK) {
        return e;
    }
    let mut pending: std::collections::HashSet<ContractInstanceId> =
        rooms.contract_ids().into_iter().collect();
    let subscribed_at = Instant::now();
    let mut last_activity = Instant::now();
    let mut status = LinkStatus::Loading;
    loop {
        let response = match socket.recv() {
            Ok(r) => {
                last_activity = Instant::now();
                Some(r)
            }
            Err(SocketError::Timeout) => None,
            Err(e) => return e,
        };
        let mut changed = false;
        match response {
            None => {
                // Quiet for a while: check the connection is still there.
                if last_activity.elapsed() >= IDLE_CHECK {
                    if let Err(e) = socket.ping() {
                        return e;
                    }
                    last_activity = Instant::now();
                }
            }
            Some(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                key,
                state,
                ..
            }))) => {
                pending.remove(key.id());
                changed = rooms.merge_state(key.id(), state.as_ref());
            }
            Some(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                key,
                update,
            }))) => changed = rooms.apply_update(key.id(), &update),
            // The network has no copy. The web UI would seed one with a PUT; this app only
            // reads, so it keeps showing the stored copy.
            Some(Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                instance_id,
            }))) => {
                pending.remove(&instance_id);
            }
            // A subscribe GET that failed leaves that room on its stored copy.
            Some(Err(e)) => {
                if let Some(id) = failed_contract(&e) {
                    pending.remove(&id);
                }
            }
            Some(Ok(_)) => {}
        }
        // "Catching up" ends when every room has answered, or after a while regardless: a
        // room nobody on the network holds may never answer, and its subscription stays
        // requested either way.
        if status == LinkStatus::Loading
            && (pending.is_empty() || subscribed_at.elapsed() >= CATCH_UP_LIMIT)
        {
            pending.clear();
            status = LinkStatus::Live;
            changed = true;
        }
        if changed {
            rooms.report(status, report);
        }
    }
}

/// The room contract a failed request names, if any.
fn failed_contract(e: &ClientError) -> Option<ContractInstanceId> {
    match e.kind() {
        ErrorKind::RequestError(RequestError::ContractError(c)) => match c {
            ContractError::Get { key, .. } | ContractError::Subscribe { key, .. } => {
                Some(*key.id())
            }
            ContractError::MissingContract { key } => Some(*key),
            _ => None,
        },
        _ => None,
    }
}

/// Read the stored rooms from the chat delegate: list the keys, then get each room and the
/// list's view preferences.
fn load(socket: &mut NodeSocket, rooms: &mut Rooms) -> Result<(), SocketError> {
    socket.set_read_timeout(DELEGATE_DEADLINE)?;
    socket.send(&delegate::request(&Read::List))?;
    let deadline = Instant::now() + DELEGATE_DEADLINE;
    let keys = loop {
        match delegate_answer(socket, deadline)? {
            Some(ChatDelegateResponseMsg::ListResponse { keys }) => break keys,
            Some(_) => continue,
            // The delegate is not on this node: River on the web never ran here.
            None => {
                rooms.replace(Vec::new(), StoredRoomsMeta::default());
                return Ok(());
            }
        }
    };
    let plan = plan(keys.iter().map(|k| k.as_bytes()));
    let mut wanted: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    for vk in &plan.rooms {
        wanted.insert(room_storage_key(vk), None);
    }
    if plan.has_meta {
        wanted.insert(ROOMS_META_KEY.to_vec(), None);
    }
    for key in wanted.keys() {
        socket.send(&delegate::request(&Read::Get(key.clone())))?;
    }
    let mut answered = 0;
    while answered < wanted.len() {
        let Some(answer) = delegate_answer(socket, deadline)? else {
            return Err(SocketError::Failed(
                "River's storage went away while reading your rooms".into(),
            ));
        };
        // Answers are matched by storage key, as the web UI matches them.
        if let ChatDelegateResponseMsg::GetResponse { key, value } = answer
            && let Some(slot) = wanted.get_mut(key.as_bytes())
            && slot.is_none()
        {
            *slot = Some(value.unwrap_or_default());
            answered += 1;
        }
    }
    let meta = match wanted.remove(ROOMS_META_KEY) {
        Some(Some(bytes)) if !bytes.is_empty() => decode_rooms_meta(&bytes).unwrap_or_else(|e| {
            eprintln!("River: {e}");
            StoredRoomsMeta::default()
        }),
        _ => StoredRoomsMeta::default(),
    };
    let mut slots = Vec::new();
    for vk in &plan.rooms {
        let Some(Some(bytes)) = wanted.remove(&room_storage_key(vk)) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        match decode_room_slot(&bytes) {
            Ok(StoredRoomSlot::Present(room)) => slots.push(*room),
            Ok(StoredRoomSlot::Tombstone) => {}
            // One unreadable room must not hide the others.
            Err(e) => eprintln!("River: skipping a stored room: {e}"),
        }
    }
    rooms.replace(slots, meta);
    Ok(())
}

/// The next chat-delegate answer. `None` when the node says the delegate is not registered.
fn delegate_answer(
    socket: &mut NodeSocket,
    deadline: Instant,
) -> Result<Option<ChatDelegateResponseMsg>, SocketError> {
    loop {
        if Instant::now() >= deadline {
            return Err(SocketError::Failed(
                "River's storage on this computer didn't answer in time".into(),
            ));
        }
        match socket.recv() {
            Ok(Ok(response)) => {
                if let Some(answer) = delegate::answers(&response).into_iter().next() {
                    return Ok(Some(answer));
                }
            }
            Ok(Err(e)) => return delegate_error(&e).map(|()| None),
            Err(SocketError::Timeout) => {}
            Err(e) => return Err(e),
        }
    }
}

/// `Ok(())` when the error only says the delegate is missing; the error to stop on otherwise.
fn delegate_error(e: &ClientError) -> Result<(), SocketError> {
    if let ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(_))) =
        e.kind()
    {
        return Ok(());
    }
    let text = e.to_string();
    if text.contains(LIST_REFUSED_MARKER) {
        return Err(SocketError::Failed(format!(
            "{REFUSED_PREFIX} to list your rooms for this app."
        )));
    }
    Err(SocketError::Failed(format!(
        "Freenet couldn't read your rooms: {text}"
    )))
}

/// What a `ListResponse` says to read: the per-room keys, and whether `rooms_meta` exists.
#[derive(Debug, PartialEq)]
struct Plan {
    rooms: Vec<VerifyingKey>,
    has_meta: bool,
}

fn plan<'a>(keys: impl Iterator<Item = &'a [u8]>) -> Plan {
    let mut plan = Plan {
        rooms: Vec::new(),
        has_meta: false,
    };
    for key in keys {
        if let Some(vk) = parse_room_storage_key(key) {
            if !plan.rooms.contains(&vk) {
                plan.rooms.push(vk);
            }
        } else if key == ROOMS_META_KEY {
            plan.has_meta = true;
        }
    }
    plan
}

/// The rooms being shown, with what is needed to keep them current.
#[derive(Default)]
struct Rooms {
    /// In the room list's order.
    list: Vec<LiveRoom>,
    open: Option<String>,
    /// Whether the rooms have been read at least once this run.
    loaded: bool,
}

impl Rooms {
    /// Take freshly read rooms. A room already followed keeps its copy, which has had the
    /// network merged in; the stored copy is never newer than that.
    fn replace(&mut self, slots: Vec<river_core::stored_rooms::StoredRoom>, meta: StoredRoomsMeta) {
        let mut old: HashMap<VerifyingKey, LiveRoom> = self
            .list
            .drain(..)
            .map(|r| (r.stored.owner_vk, r))
            .collect();
        let mut by_key: HashMap<VerifyingKey, LiveRoom> = slots
            .into_iter()
            .map(|s| {
                let vk = s.owner_vk;
                (vk, old.remove(&vk).unwrap_or_else(|| LiveRoom::new(s)))
            })
            .collect();
        for vk in order(&meta.room_order, by_key.keys().copied().collect()) {
            if let Some(room) = by_key.remove(&vk) {
                self.list.push(room);
            }
        }
        self.open = meta
            .current_room_key
            .filter(|k| self.list.iter().any(|r| r.stored.owner_vk == *k))
            .map(|k| bs58::encode(k.to_bytes()).into_string());
        self.loaded = true;
    }

    fn contract_ids(&self) -> Vec<ContractInstanceId> {
        self.list.iter().map(LiveRoom::contract_id).collect()
    }

    fn by_contract(&mut self, id: &ContractInstanceId) -> Option<&mut LiveRoom> {
        self.list.iter_mut().find(|r| r.contract_id() == *id)
    }

    fn merge_state(&mut self, id: &ContractInstanceId, bytes: &[u8]) -> bool {
        let Some(room) = self.by_contract(id) else {
            return false;
        };
        let Ok(incoming) = ciborium::de::from_reader::<ChatRoomStateV1, _>(bytes) else {
            eprintln!("River: a room's state from the network could not be read");
            return false;
        };
        match room.merge_state(&incoming) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("River: a room's state from the network was not merged: {e}");
                false
            }
        }
    }

    fn apply_update(&mut self, id: &ContractInstanceId, update: &UpdateData<'_>) -> bool {
        match update {
            UpdateData::State(state) => self.merge_state(id, state.as_ref()),
            UpdateData::StateAndDelta { state, .. } => self.merge_state(id, state.as_ref()),
            UpdateData::Delta(delta) => {
                let Some(room) = self.by_contract(id) else {
                    return false;
                };
                let Ok(delta) =
                    ciborium::de::from_reader::<ChatRoomStateV1Delta, _>(delta.as_ref())
                else {
                    eprintln!("River: a room's update from the network could not be read");
                    return false;
                };
                match room.apply_delta(delta) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("River: a room's update from the network was not applied: {e}");
                        false
                    }
                }
            }
            // Related-contract updates, and variants newer than this stdlib.
            _ => false,
        }
    }

    fn views(&self) -> Vec<Room> {
        self.list.iter().map(|r| r.view(&local_minutes)).collect()
    }

    fn report(&self, status: LinkStatus, report: &dyn Fn(RoomsUpdate)) {
        report(RoomsUpdate::Rooms {
            rooms: self.views(),
            open: self.open.clone(),
            status,
            now: local_minutes(SystemTime::now()),
        });
    }

    /// The connection is gone: keep showing the rooms if there are any, and say so.
    fn lost(&self, why: &str, report: &dyn Fn(RoomsUpdate)) {
        if self.loaded {
            report(RoomsUpdate::Status(LinkStatus::Reconnecting));
        } else {
            report(RoomsUpdate::Access(Access::Unreachable(why.to_owned())));
        }
    }
}

/// The web UI's room order (`Rooms::ordered_room_keys`): the stored order first, then the
/// rest by key bytes.
fn order(room_order: &[VerifyingKey], mut present: Vec<VerifyingKey>) -> Vec<VerifyingKey> {
    let mut out: Vec<VerifyingKey> = Vec::new();
    for k in room_order {
        if present.contains(k) && !out.contains(k) {
            out.push(*k);
        }
    }
    present.retain(|k| !out.contains(k));
    present.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    out.extend(present);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn vk(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    #[test]
    fn the_plan_reads_room_keys_and_meta_and_ignores_the_rest() {
        let a = room_storage_key(&vk(1));
        let b = room_storage_key(&vk(2));
        let keys: Vec<&[u8]> = vec![
            &a,
            b"rooms_meta",
            b"rooms_data",
            b"signing_key:xyz",
            b"room:not-base58!",
            &b,
            &a,
        ];
        assert_eq!(
            plan(keys.into_iter()),
            Plan {
                rooms: vec![vk(1), vk(2)],
                has_meta: true
            }
        );
        assert_eq!(
            plan(std::iter::empty()),
            Plan {
                rooms: vec![],
                has_meta: false
            }
        );
    }

    #[test]
    fn failed_gets_name_their_room() {
        let id = ContractInstanceId::new([7; 32]);
        let key = freenet_stdlib::prelude::ContractKey::from_id_and_code(
            id,
            freenet_stdlib::prelude::CodeHash::new([1; 32]),
        );
        let get: ClientError =
            ErrorKind::RequestError(RequestError::ContractError(ContractError::Get {
                key,
                cause: "no peers".into(),
            }))
            .into();
        assert_eq!(failed_contract(&get), Some(id));
        let missing: ClientError = ErrorKind::RequestError(RequestError::ContractError(
            ContractError::MissingContract { key: id },
        ))
        .into();
        assert_eq!(failed_contract(&missing), Some(id));
        assert_eq!(failed_contract(&ErrorKind::NodeUnavailable.into()), None);
    }

    #[test]
    fn rooms_are_ordered_like_the_web_ui() {
        let (a, b, c) = (vk(1), vk(2), vk(3));
        let mut rest = vec![a, c];
        rest.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
        let mut expected = vec![b];
        expected.extend(rest);
        // An ordered key that is not present (a left room) is skipped.
        assert_eq!(order(&[vk(9), b], vec![a, b, c]), expected);
        let mut all = vec![a, b, c];
        all.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
        assert_eq!(order(&[], vec![c, b, a]), all);
    }

    #[test]
    fn a_missing_delegate_means_no_rooms_and_a_refused_list_stops() {
        let missing: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
            DelegateError::Missing(delegate::chat_delegate_key().clone()),
        ))
        .into();
        assert_eq!(delegate_error(&missing), Ok(()));
        let refused: ClientError = ErrorKind::Unhandled {
            cause: format!("{LIST_REFUSED_MARKER}: origin").into(),
        }
        .into();
        let Err(SocketError::Failed(m)) = delegate_error(&refused) else {
            panic!("a refused list is an error");
        };
        assert!(m.starts_with(REFUSED_PREFIX), "{m}");
        let other: ClientError = ErrorKind::NodeUnavailable.into();
        assert!(matches!(
            delegate_error(&other),
            Err(SocketError::Failed(_))
        ));
    }
}
