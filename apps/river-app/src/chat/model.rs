//! What the chat screen shows, as plain data.
//!
//! These are view models local to this crate. They are shaped after River's room state
//! (`common/src/room_state/`) but deliberately do not depend on `river-core` yet: the screen is
//! built and tested against example data first, and wiring it to a node is a later step.

/// A member's id within a room. In River this is derived from their verifying key.
pub type MemberId = u32;

/// A message's id within a room.
pub type MessageId = u32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: MemberId,
    pub nickname: String,
    pub role: Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    Member,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    /// Who reacted, in the order they did.
    pub by: Vec<MemberId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyTo {
    pub message: MessageId,
    pub author: MemberId,
    /// A preview of the message replied to, as River stores it beside the reply.
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub author: MemberId,
    /// Minutes since the start of the example's first day. Example data needs no calendar:
    /// the view only shows a day heading and a time of day.
    pub time: u32,
    pub text: String,
    pub reply_to: Option<ReplyTo>,
    pub reactions: Vec<Reaction>,
    pub edited: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Room {
    /// Stable across renames; River uses the owner's verifying key.
    pub key: String,
    pub name: String,
    pub description: String,
    pub private: bool,
    pub members: Vec<Member>,
    pub messages: Vec<Message>,
    /// Which member this device is in the room.
    pub me: MemberId,
    /// Messages that arrived since this device last read the room.
    pub unread: usize,
}

impl Room {
    pub fn member(&self, id: MemberId) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    pub fn nickname(&self, id: MemberId) -> &str {
        self.member(id).map_or("Unknown", |m| m.nickname.as_str())
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.messages.last()
    }

    fn next_message_id(&self) -> MessageId {
        self.messages.iter().map(|m| m.id + 1).max().unwrap_or(1)
    }

    /// Append a message from this device. `time` never runs backwards, so the history stays
    /// in order even if the clock does.
    pub fn send(&mut self, text: &str, time: u32) -> MessageId {
        let id = self.next_message_id();
        let time = self.last_message().map_or(time, |m| time.max(m.time));
        self.messages.push(Message {
            id,
            author: self.me,
            time,
            text: text.to_owned(),
            reply_to: None,
            reactions: Vec::new(),
            edited: false,
        });
        id
    }
}

/// Consecutive messages by one author, shown under one avatar and name, as the web UI groups
/// them: a new group starts on a new author, a new day, or a gap of more than five minutes.
pub struct Group<'a> {
    pub author: MemberId,
    pub messages: &'a [Message],
}

pub const GROUP_GAP_MINUTES: u32 = 5;
pub const MINUTES_PER_DAY: u32 = 24 * 60;

pub fn groups(messages: &[Message]) -> Vec<Group<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=messages.len() {
        let split = i == messages.len() || {
            let (a, b) = (&messages[i - 1], &messages[i]);
            a.author != b.author
                || a.time / MINUTES_PER_DAY != b.time / MINUTES_PER_DAY
                || b.time.saturating_sub(a.time) > GROUP_GAP_MINUTES
        };
        if split {
            out.push(Group {
                author: messages[start].author,
                messages: &messages[start..i],
            });
            start = i;
        }
    }
    out
}

/// "14:05" for a message time.
pub fn clock(time: u32) -> String {
    let m = time % MINUTES_PER_DAY;
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// The heading over a day's messages, relative to `today` (a day number).
pub fn day_label(day: u32, today: u32) -> String {
    match today.saturating_sub(day) {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        n => format!("{n} days ago"),
    }
}

/// The byte ranges of web links (`https://` or `http://` up to the next space) in `text`, with
/// trailing punctuation left out so "see https://freenet.org." links only the address.
pub fn links(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = ["https://", "http://"]
        .iter()
        .filter_map(|p| text[from..].find(p))
        .min()
    {
        let start = from + i;
        let mut end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |n| start + n);
        while end > start && text[..end].ends_with(['.', ',', ')', '!', '?', ':', ';']) {
            end -= 1;
        }
        if end > start + "https://".len() {
            out.push(start..end);
        }
        from = end.max(start + 1);
    }
    out
}

/// Up to two initials from a nickname: "Ada Lovelace" is "AL", "nova" is "N".
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace().filter_map(|w| w.chars().next());
    let first = words.next();
    let second = words.next();
    first
        .into_iter()
        .chain(second)
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u32, author: u32, time: u32) -> Message {
        Message {
            id,
            author,
            time,
            text: String::new(),
            reply_to: None,
            reactions: vec![],
            edited: false,
        }
    }

    #[test]
    fn groups_split_on_author_gap_and_day() {
        let day = MINUTES_PER_DAY;
        let m = [
            msg(1, 1, 600),
            msg(2, 1, 602),
            msg(3, 2, 603),
            msg(4, 2, 620),
            msg(5, 2, day + 1),
        ];
        let sizes: Vec<usize> = groups(&m).iter().map(|g| g.messages.len()).collect();
        assert_eq!(sizes, [2, 1, 1, 1]);
        assert!(groups(&[]).is_empty());
    }

    #[test]
    fn initials_and_times() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("nova"), "N");
        assert_eq!(initials("  "), "");
        assert_eq!(clock(14 * 60 + 5), "14:05");
        assert_eq!(clock(MINUTES_PER_DAY + 61), "01:01");
        assert_eq!(day_label(3, 3), "Today");
        assert_eq!(day_label(2, 3), "Yesterday");
        assert_eq!(day_label(0, 3), "3 days ago");
    }

    #[test]
    fn links_stop_at_spaces_and_trailing_punctuation() {
        let t = "see https://freenet.org/quickstart. or (http://a.example/x), done";
        let found: Vec<&str> = links(t).into_iter().map(|r| &t[r]).collect();
        assert_eq!(
            found,
            ["https://freenet.org/quickstart", "http://a.example/x"]
        );
        assert!(links("no links, just https:// alone").is_empty());
    }

    #[test]
    fn sending_never_goes_back_in_time() {
        let mut room = Room {
            key: "r".into(),
            name: "r".into(),
            description: String::new(),
            private: false,
            members: vec![],
            messages: vec![msg(7, 2, 900)],
            me: 1,
            unread: 0,
        };
        let id = room.send("hi", 100);
        assert_eq!(id, 8);
        let last = room.last_message().unwrap();
        assert_eq!((last.author, last.time, last.text.as_str()), (1, 900, "hi"));
    }
}
