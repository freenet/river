//! Example rooms, members and messages, shown until the app reads real rooms from a node.
//!
//! The content is invented. It is meant to look like a lived-in River account: a busy public
//! room, a private room, a quiet room, a brand new empty room, and messages that exercise every
//! part of the conversation view (groups, a reply, reactions, a long wrapping message, a link,
//! an edit).

use super::model::{
    self, Appointer, Details, Editable, Limits, Member, MemberId, Message, Reaction, ReplyTo, Role,
    Room, RotateOffer, Usage,
};

pub struct ExampleData {
    pub rooms: Vec<Room>,
    /// The day number the example treats as today.
    pub today: u32,
    /// The date of day 0, in days since 1970-01-01: today is Friday 6 November 2026.
    pub day_zero: i64,
    /// The time stamped on a message sent now.
    pub now: u32,
}

const TODAY: u32 = 3;

/// Minutes for `h:m` on `day`.
const fn at(day: u32, h: u32, m: u32) -> u32 {
    day * 24 * 60 + h * 60 + m
}

fn member(id: MemberId, nickname: &str, role: Role) -> Member {
    Member::new(id, nickname, role)
}

/// `m`, invited by `by`.
fn invited(mut m: Member, by: MemberId) -> Member {
    m.invited_by = Some(by);
    m
}

/// `m` as a deputy in the viewer's view: appointed by `by`, and able (or not) to ban the viewer.
fn deputy(mut m: Member, by: &[Appointer], can_ban_you: bool) -> Member {
    m.role = Role::Deputy;
    m.deputized_by = by.to_vec();
    m.can_ban_you = Some(can_ban_you);
    m.my_deputy = by.contains(&Appointer::You);
    m
}

struct Builder {
    messages: Vec<Message>,
}

impl Builder {
    fn new() -> Self {
        Builder {
            messages: Vec::new(),
        }
    }

    fn say(&mut self, author: MemberId, time: u32, text: &str) -> &mut Message {
        let id = self.messages.len() as u32 + 1;
        self.messages.push(Message {
            id,
            key: id.to_string(),
            author,
            time,
            text: text.into(),
            source: text.into(),
            md: text.into(),
            reply_to: None,
            reactions: Vec::new(),
            edited: false,
            pending: None,
        });
        self.messages.last_mut().unwrap()
    }
}

trait MessageExt {
    fn react(&mut self, emoji: &str, by: &[MemberId]) -> &mut Self;
    fn replying_to(&mut self, message: u32, author: MemberId, preview: &str) -> &mut Self;
    fn edited(&mut self) -> &mut Self;
}

impl MessageExt for Message {
    fn react(&mut self, emoji: &str, by: &[MemberId]) -> &mut Self {
        self.reactions.push(Reaction {
            emoji: emoji.into(),
            by: by.to_vec(),
        });
        self
    }
    fn replying_to(&mut self, message: u32, author: MemberId, preview: &str) -> &mut Self {
        self.reply_to = Some(ReplyTo {
            message,
            author,
            preview: preview.into(),
        });
        self
    }
    fn edited(&mut self) -> &mut Self {
        self.edited = true;
        self
    }
}

// Member ids, shared across rooms so one person keeps one avatar colour. They are chosen so
// the seven people get seven different hues (see `style::avatar_colors`).
const ME: MemberId = 21;
const MAYA: MemberId = 12;
const JONAS: MemberId = 30;
const PRIYA: MemberId = 27;
const TOMAS: MemberId = 15;
const ELENA: MemberId = 16;
const KWAME: MemberId = 19;

fn town_square() -> Room {
    let mut b = Builder::new();
    b.say(
        JONAS,
        at(TODAY - 1, 18, 2),
        "Anyone else running a node on a Raspberry Pi? Mine has been up for nine days straight.",
    );
    b.say(
        PRIYA,
        at(TODAY - 1, 18, 9),
        "Pi 5 here, no complaints. It sits behind my router and just works.",
    )
    .react("🙌\u{fe0f}", &[JONAS, ME]);
    b.say(
        MAYA,
        at(TODAY, 9, 14),
        "Morning all! The new release went out overnight.",
    );
    b.say(
        MAYA,
        at(TODAY, 9, 15),
        "Release notes are at https://freenet.org/news if you want the details.",
    )
    .react("🎉", &[JONAS, PRIYA, ELENA, ME])
    .react("🚀", &[TOMAS, KWAME]);
    b.say(
        TOMAS,
        at(TODAY, 9, 31),
        "Updated, and the room list loads noticeably faster for me.",
    );
    b.say(ELENA, at(TODAY, 9, 40), "Question for the people who have been here a while: when I invite someone, do they need to be online at the same time as me for the invitation to work, or can they accept it later? I sent one to a friend yesterday and I am not sure whether I should resend it once she is back from her trip next week.");
    b.say(MAYA, at(TODAY, 9, 46), "They can accept it whenever they like. The invitation carries everything they need, and the room lives on the network, not on your computer.")
        .replying_to(6, ELENA, "Question for the people who have been here a while: when I invite someone…")
        .react("❤️", &[ELENA])
        .react("👍\u{fe0f}", &[TOMAS, ME]);
    b.say(ELENA, at(TODAY, 9, 47), "Perfect, thank you!");
    b.say(
        KWAME,
        at(TODAY, 10, 3),
        "Small thing I noticed: nicknames are encrypted in private rooms too. Nice touch.",
    )
    .edited();
    b.say(
        ME,
        at(TODAY, 10, 12),
        "That's right, everything except the membership list itself.",
    );
    b.say(
        ME,
        at(TODAY, 10, 12),
        "Which reminds me, I should finally write up how room secrets get rotated.",
    )
    .react("👀\u{fe0f}", &[KWAME, PRIYA]);
    Room {
        key: "town-square".into(),
        name: "Town Square".into(),
        description: "The public room for everyone using Freenet. Be kind.".into(),
        private: false,
        members: vec![
            member(MAYA, "Maya Chen", Role::Owner),
            // Jonas is a deputy of the owner: he can ban Sam. Elena is Sam's own deputy.
            deputy(
                invited(member(JONAS, "Jonas", Role::Member), MAYA),
                &[Appointer::Owner],
                true,
            ),
            invited(member(PRIYA, "Priya Raman", Role::Member), MAYA),
            invited(member(TOMAS, "Tomás", Role::Member), JONAS),
            deputy(
                invited(member(ELENA, "Elena Petrova", Role::Member), PRIYA),
                &[Appointer::You],
                false,
            ),
            invited(member(KWAME, "Kwame Mensah", Role::Member), JONAS),
            invited(member(ME, "Sam Rivers", Role::Member), PRIYA),
        ],
        messages: b.messages,
        me: ME,
        unread: 0,
        send_block: None,
        not_member: false,
        notices: Vec::new(),
        nickname_limit: 50,
        own_nickname: None,
        nickname_block: None,
        deputy_changes: Vec::new(),
        deputy_block: None,
        deputy_grant_block: None,
        ban_offers: Default::default(),
        ban_preview: Default::default(),
        deputies_full: false,
        wraps: None,
        details: Details::default(),
    }
}

fn design_crew() -> Room {
    let mut b = Builder::new();
    b.say(
        PRIYA,
        at(TODAY - 2, 16, 20),
        "I pushed the new mockups for the members panel to our shared folder.",
    );
    b.say(
        ME,
        at(TODAY - 2, 16, 41),
        "These look great. The owner badge reads much better now.",
    )
    .react("😊\u{fe0f}", &[PRIYA]);
    b.say(
        KWAME,
        at(TODAY, 8, 55),
        "Can we look at the composer next? It feels cramped on small screens.",
    );
    b.say(
        PRIYA,
        at(TODAY, 9, 2),
        "Agreed. I'll sketch a version with the send button inside the field.",
    )
    .replying_to(
        3,
        KWAME,
        "Can we look at the composer next? It feels cramped on small screens.",
    );
    Room {
        key: "design-crew".into(),
        name: "Design Crew".into(),
        description: "Private: sketches and feedback before anything goes public.".into(),
        private: true,
        members: vec![
            member(ME, "Sam Rivers", Role::Owner),
            member(PRIYA, "Priya Raman", Role::Member),
            member(KWAME, "Kwame Mensah", Role::Member),
        ],
        messages: b.messages,
        me: ME,
        unread: 2,
        send_block: None,
        not_member: false,
        notices: Vec::new(),
        nickname_limit: 50,
        own_nickname: None,
        nickname_block: None,
        deputy_changes: Vec::new(),
        deputy_block: None,
        deputy_grant_block: None,
        ban_offers: Default::default(),
        ban_preview: Default::default(),
        deputies_full: false,
        wraps: None,
        details: Details::default(),
    }
}

fn rust_help() -> Room {
    let mut b = Builder::new();
    b.say(
        JONAS,
        at(TODAY, 7, 48),
        "Is there a way to run a contract's tests without a node?",
    );
    b.say(
        TOMAS,
        at(TODAY, 7, 52),
        "Yes, they're ordinary Rust unit tests. cargo test in the contract's folder.",
    );
    b.say(
        JONAS,
        at(TODAY, 7, 53),
        "Oh, that's easier than I expected. Thanks!",
    )
    .react("👍\u{fe0f}", &[TOMAS]);
    Room {
        key: "rust-help".into(),
        name: "Rust Help".into(),
        description: "Questions about building contracts and delegates.".into(),
        private: false,
        members: vec![
            member(TOMAS, "Tomás", Role::Owner),
            member(JONAS, "Jonas", Role::Member),
            member(ME, "Sam", Role::Member),
        ],
        messages: b.messages,
        me: ME,
        unread: 2,
        send_block: None,
        not_member: false,
        notices: Vec::new(),
        nickname_limit: 50,
        own_nickname: None,
        nickname_block: None,
        deputy_changes: Vec::new(),
        deputy_block: None,
        deputy_grant_block: None,
        ban_offers: Default::default(),
        ban_preview: Default::default(),
        deputies_full: false,
        wraps: None,
        details: Details::default(),
    }
}

fn family() -> Room {
    let mut b = Builder::new();
    b.say(
        ELENA,
        at(TODAY - 1, 20, 15),
        "Photos from Sunday are up. Grandma looks so happy in the garden ones.",
    )
    .react("❤️", &[ME]);
    Room {
        key: "family".into(),
        name: "Family".into(),
        description: "Just us.".into(),
        private: true,
        members: vec![
            member(ELENA, "Elena", Role::Owner),
            member(ME, "Sam", Role::Member),
        ],
        messages: b.messages,
        me: ME,
        unread: 0,
        send_block: None,
        not_member: false,
        notices: Vec::new(),
        nickname_limit: 50,
        own_nickname: None,
        nickname_block: None,
        deputy_changes: Vec::new(),
        deputy_block: None,
        deputy_grant_block: None,
        ban_offers: Default::default(),
        ban_preview: Default::default(),
        deputies_full: false,
        wraps: None,
        details: Details::default(),
    }
}

fn book_club() -> Room {
    Room {
        key: "book-club".into(),
        name: "Book Club".into(),
        description: "This month: The Dispossessed.".into(),
        private: false,
        members: vec![member(ME, "Sam Rivers", Role::Owner)],
        messages: Vec::new(),
        me: ME,
        unread: 0,
        send_block: None,
        not_member: false,
        notices: Vec::new(),
        nickname_limit: 50,
        own_nickname: None,
        nickname_block: None,
        deputy_changes: Vec::new(),
        deputy_block: None,
        deputy_grant_block: None,
        ban_offers: Default::default(),
        ban_preview: Default::default(),
        deputies_full: false,
        wraps: None,
        details: Details::default(),
    }
}

/// The details dialog's content for an example room: the web UI's default limits, and a key
/// made from the room's own.
fn with_details(mut room: Room) -> Room {
    let owner = room
        .members
        .iter()
        .any(|m| m.role == Role::Owner && m.id == room.me);
    room.details = Details {
        contract_id: format!("{:0<44}", room.key.replace('-', "")),
        owner_key: format!("{:0<44}", format!("{}owner", room.key.replace('-', ""))),
        limits: Limits {
            max_members: 200,
            max_recent_messages: 100,
            max_message_size: 1000,
            max_user_bans: 10,
            max_nickname_size: 50,
            max_room_name: 100,
            max_room_description: 500,
        },
        usage: Usage {
            members: room
                .members
                .iter()
                .filter(|m| m.role != Role::Owner)
                .count(),
            bans: 0,
            messages: room.messages.len(),
        },
        secret_version: 0,
        name_source: room.name.clone(),
        description_source: room.description.clone(),
        editable: if owner { Editable::Yes } else { Editable::No },
        pending: None,
        rotate: if owner && room.private {
            RotateOffer::Allowed
        } else {
            RotateOffer::No
        },
        rotation: None,
    };
    room
}

pub fn data() -> ExampleData {
    ExampleData {
        rooms: vec![
            with_details(town_square()),
            with_details(design_crew()),
            with_details(rust_help()),
            with_details(family()),
            with_details(book_club()),
        ],
        today: TODAY,
        day_zero: model::days_from_civil(2026, 11, 6) - TODAY as i64,
        now: at(TODAY, 10, 14),
    }
}

/// The key of the example's private room, for scenes and tests.
pub const PRIVATE_ROOM: &str = "design-crew";
/// The key of the example's room with no messages.
pub const EMPTY_ROOM: &str = "book-club";

// Direct messages ------------------------------------------------------------------------------

/// Unix seconds for `h:m` UTC on example day `day`, from the example's own calendar.
fn dm_secs(day: u32, h: u32, m: u32) -> u64 {
    let day0 = model::days_from_civil(2026, 11, 6) - i64::from(TODAY);
    ((day0 + i64::from(day)) as u64) * 86_400 + u64::from(h * 3600 + m * 60)
}

fn dm_message(
    key: &str,
    outgoing: bool,
    at: u64,
    body: super::dm_model::DmBody,
) -> super::dm_model::DmMessage {
    super::dm_model::DmMessage {
        key: key.into(),
        outgoing,
        timestamp: at,
        body,
    }
}

fn dm_thread(
    room: &str,
    room_name: &str,
    peer: MemberId,
    peer_name: &str,
    messages: Vec<super::dm_model::DmMessage>,
) -> super::dm_model::DmThread {
    let last_ts = messages.iter().map(|m| m.timestamp).max().unwrap_or(0);
    let last_inbound_ts = messages
        .iter()
        .filter(|m| !m.outgoing)
        .map(|m| m.timestamp)
        .max()
        .unwrap_or(0);
    super::dm_model::DmThread {
        room: room.into(),
        room_name: room_name.into(),
        peer_uid: peer.to_string(),
        peer: river_core::room_state::member::MemberId(freenet_scaffold::util::FastHash(
            i64::from(peer),
        )),
        peer_name: peer_name.into(),
        peer_is_member: true,
        messages,
        last_ts,
        last_inbound_ts,
    }
}

/// Example direct messages: three conversations in two rooms, with markdown, an invitation and
/// a message the user sent.
pub fn dm_snapshot() -> super::dm_model::DmSnapshot {
    use super::dm_model::DmBody::{Invite, Placeholder, SentInvite, Text};
    use super::dm_model::Placeholder as Why;
    let priya = dm_thread(
        "town-square",
        "Town Square",
        PRIYA,
        "Priya Raman",
        vec![
            dm_message(
                "p1",
                false,
                dm_secs(2, 16, 5),
                Text("Did you see the **new** release notes?".into()),
            ),
            dm_message(
                "p2",
                true,
                dm_secs(2, 16, 9),
                Text("Not yet, send me the link.".into()),
            ),
            dm_message(
                "p3",
                false,
                dm_secs(TODAY, 9, 40),
                Text(
                    "Here: https://freenet.org/ and the `river` notes.\n\n- fast\n- private".into(),
                ),
            ),
            dm_message(
                "p4",
                false,
                dm_secs(TODAY, 9, 41),
                Text("Let me know what you think.".into()),
            ),
            dm_message(
                "p5",
                true,
                dm_secs(TODAY, 9, 50),
                Placeholder(Why::CiphertextOnly),
            ),
        ],
    );
    let kwame = dm_thread(
        "design-crew",
        "Design Crew",
        KWAME,
        "Kwame Mensah",
        vec![
            dm_message(
                "k1",
                false,
                dm_secs(TODAY, 8, 30),
                Invite {
                    room_label: "Rust Help".into(),
                    note: Some("Come and help us review the **parser** patch.".into()),
                },
            ),
            dm_message(
                "k2",
                true,
                dm_secs(TODAY, 8, 45),
                SentInvite {
                    note: Some("Welcome to the crew".into()),
                },
            ),
        ],
    );
    let maya = dm_thread(
        "town-square",
        "Town Square",
        MAYA,
        "Maya Chen",
        vec![dm_message(
            "m1",
            false,
            dm_secs(1, 11, 0),
            Placeholder(Why::CannotDecrypt),
        )],
    );
    super::dm_model::DmSnapshot {
        threads: [priya, kwame, maya].map(std::sync::Arc::new).to_vec(),
        room_keys: dm_room_keys(),
        truncated: Vec::new(),
        hydrated: true,
    }
}

/// The key of the thread the example opens: Priya in the Town Square.
pub const DM_PEER: &str = "27";

/// The keys of the example rooms.
pub fn dm_room_keys() -> Vec<String> {
    data().rooms.into_iter().map(|r| r.key).collect()
}
