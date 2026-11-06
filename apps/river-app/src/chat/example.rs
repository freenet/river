//! Example rooms, members and messages, shown until the app reads real rooms from a node.
//!
//! The content is invented. It is meant to look like a lived-in River account: a busy public
//! room, a private room, a quiet room, a brand new empty room, and messages that exercise every
//! part of the conversation view (groups, a reply, reactions, a long wrapping message, a link,
//! an edit).

use super::model::{Member, MemberId, Message, Reaction, ReplyTo, Role, Room};

pub struct ExampleData {
    pub rooms: Vec<Room>,
    /// The day number the example treats as today.
    pub today: u32,
    /// The time stamped on a message sent now.
    pub now: u32,
}

const TODAY: u32 = 3;

/// Minutes for `h:m` on `day`.
const fn at(day: u32, h: u32, m: u32) -> u32 {
    day * 24 * 60 + h * 60 + m
}

fn member(id: MemberId, nickname: &str, role: Role) -> Member {
    Member {
        id,
        nickname: nickname.into(),
        role,
    }
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
            author,
            time,
            text: text.into(),
            reply_to: None,
            reactions: Vec::new(),
            edited: false,
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

// Member ids, shared across rooms so one person keeps one avatar colour.
const ME: MemberId = 11;
const MAYA: MemberId = 12;
const JONAS: MemberId = 13;
const PRIYA: MemberId = 14;
const TOMAS: MemberId = 15;
const ELENA: MemberId = 16;
const KWAME: MemberId = 17;

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
            member(JONAS, "Jonas", Role::Member),
            member(PRIYA, "Priya Raman", Role::Member),
            member(TOMAS, "Tomás", Role::Member),
            member(ELENA, "Elena Petrova", Role::Member),
            member(KWAME, "Kwame Mensah", Role::Member),
            member(ME, "Sam Rivers", Role::Member),
        ],
        messages: b.messages,
        me: ME,
        unread: 0,
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
        unread: 5,
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
    }
}

pub fn data() -> ExampleData {
    ExampleData {
        rooms: vec![
            town_square(),
            design_crew(),
            rust_help(),
            family(),
            book_club(),
        ],
        today: TODAY,
        now: at(TODAY, 10, 14),
    }
}

/// The key of the example's private room, for scenes and tests.
pub const PRIVATE_ROOM: &str = "design-crew";
/// The key of the example's room with no messages.
pub const EMPTY_ROOM: &str = "book-club";
