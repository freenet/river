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

/// A message of yours that the node has not taken yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Its entry in the outbox, for Retry and Discard.
    pub entry: u64,
    /// Why it was not sent; `None` while it is still being sent.
    pub failed: Option<String>,
    /// Whether Retry can send it again.
    pub retry: bool,
}

/// Something you did that could not be sent and has no message row to say so on (an edit, a
/// deletion, a reaction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub entry: u64,
    pub text: String,
    /// Whether Retry can send it again (not when what it acted on is gone).
    pub retry: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    /// Stable across updates, unlike `id`: what an action on this message names.
    pub key: String,
    pub author: MemberId,
    /// Minutes since the start of day 0 of message time. The chat knows which calendar date
    /// day 0 is (`Chat::day_zero`), which the day headings use.
    pub time: u32,
    pub text: String,
    /// The text as written, which an edit starts from: mention tokens (`@[name](rv:id)`) as
    /// they are stored, where `text` shows names. Empty when this computer can't read the
    /// message, which then can't be edited.
    pub source: String,
    pub reply_to: Option<ReplyTo>,
    pub reactions: Vec<Reaction>,
    pub edited: bool,
    /// `Some` for a message of yours still on its way, or one that failed.
    pub pending: Option<Pending>,
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
    /// Why you can't send here, if you can't: the composer says it instead of taking text.
    pub send_block: Option<String>,
    /// Edits, deletions and reactions of yours that could not be sent.
    pub notices: Vec<Notice>,
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
        self.messages
            .iter()
            .map(|m| m.id.saturating_add(1))
            .max()
            .unwrap_or(1)
    }

    /// Append a message from this device. `time` never runs backwards, so the history stays
    /// in order even if the clock does.
    pub fn send(&mut self, text: &str, time: u32) -> MessageId {
        let id = self.next_message_id();
        let time = self.last_message().map_or(time, |m| time.max(m.time));
        self.messages.push(Message {
            id,
            key: id.to_string(),
            author: self.me,
            time,
            text: text.to_owned(),
            source: text.to_owned(),
            reply_to: None,
            reactions: Vec::new(),
            edited: false,
            pending: None,
        });
        id
    }

    pub fn message_by_key(&self, key: &str) -> Option<&Message> {
        self.messages.iter().find(|m| m.key == key)
    }

    /// A reply from this device to the message keyed `target`, in memory (example rooms).
    pub fn reply(&mut self, text: &str, time: u32, target: &str) -> Option<MessageId> {
        let t = self.message_by_key(target)?;
        let reply = ReplyTo {
            message: t.id,
            author: t.author,
            preview: preview(&t.text, 100),
        };
        let id = self.send(text, time);
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
            m.reply_to = Some(reply);
        }
        Some(id)
    }

    /// Change the text of your message keyed `target`, in memory (example rooms).
    pub fn edit(&mut self, target: &str, text: &str) -> bool {
        let me = self.me;
        match self
            .messages
            .iter_mut()
            .find(|m| m.key == target && m.author == me)
        {
            Some(m) => {
                m.text = text.to_owned();
                m.source = text.to_owned();
                m.edited = true;
                true
            }
            None => false,
        }
    }

    /// Remove your message keyed `target`, in memory (example rooms).
    pub fn delete(&mut self, target: &str) -> bool {
        let me = self.me;
        let before = self.messages.len();
        self.messages
            .retain(|m| !(m.key == target && m.author == me));
        self.messages.len() != before
    }

    /// Your reaction on the message keyed `target`, if any.
    pub fn own_reaction(&self, target: &str) -> Option<&str> {
        self.message_by_key(target)?
            .reactions
            .iter()
            .find(|r| r.by.contains(&self.me))
            .map(|r| r.emoji.as_str())
    }

    /// Toggle your reaction on the message keyed `target`, in memory (example rooms), as River
    /// does: one reaction per member per message, so a different emoji replaces yours.
    pub fn react(&mut self, target: &str, emoji: &str) -> bool {
        let me = self.me;
        let Some(m) = self.messages.iter_mut().find(|m| m.key == target) else {
            return false;
        };
        let same = m
            .reactions
            .iter()
            .any(|r| r.emoji == emoji && r.by.contains(&me));
        for r in &mut m.reactions {
            r.by.retain(|b| *b != me);
        }
        if !same {
            match m.reactions.iter_mut().find(|r| r.emoji == emoji) {
                Some(r) => r.by.push(me),
                None => m.reactions.push(Reaction {
                    emoji: emoji.to_owned(),
                    by: vec![me],
                }),
            }
        }
        m.reactions.retain(|r| !r.by.is_empty());
        true
    }

    /// How many messages in the room someone else wrote: the most `unread` can honestly be.
    pub fn from_others(&self) -> usize {
        self.messages.iter().filter(|m| m.author != self.me).count()
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

/// Days since 1970-01-01 for a calendar date (Howard Hinnant's `days_from_civil`).
pub const fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The calendar date `(year, month, day)` of a count of days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

const WEEKDAYS: [&str; 7] = [
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The heading over a day's messages, as the web UI writes it (`format_date_separator_label`
/// in `ui/src/util.rs`): "Today", "Yesterday", "Monday, June 3", and the year too when it is
/// not this year's. Both days count from 1970-01-01.
pub fn day_label(day: i64, today: i64) -> String {
    match today - day {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => {
            let (y, m, d) = civil_from_days(day);
            let weekday = WEEKDAYS[day.rem_euclid(7) as usize];
            let month = MONTHS[m as usize - 1];
            if y == civil_from_days(today).0 {
                format!("{weekday}, {month} {d}")
            } else {
                format!("{weekday}, {month} {d}, {y}")
            }
        }
    }
}

/// A one-line preview of a message for the room list: whitespace runs (newlines included)
/// become one space, and past `max_chars` it is cut at a word boundary, with trailing
/// punctuation dropped before the "…" so a cut never reads as "up.…".
pub fn preview(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars).collect();
    // A cut that lands just before a space already ends on a whole word.
    let at_word_end = flat.chars().nth(max_chars).is_some_and(char::is_whitespace);
    let cut = match cut.rfind(' ') {
        _ if at_word_end => cut.as_str(),
        Some(i) if i > 0 => &cut[..i],
        _ => cut.as_str(),
    };
    let cut = cut.trim_end_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
    format!("{cut}…")
}

/// The byte ranges of web links (`https://` or `http://` up to the next space) in `text`, with
/// trailing punctuation left out so "see https://freenet.org." links only the address. A
/// closing ")" stays when the address itself opened one, as Wikipedia links do.
pub fn links(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some((i, scheme)) = ["https://", "http://"]
        .iter()
        .filter_map(|p| text[from..].find(p).map(|i| (i, p.len())))
        .min()
    {
        let start = from + i;
        let mut end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |n| start + n);
        loop {
            let s = &text[start..end];
            let trim = s.ends_with(['.', ',', '!', '?', ':', ';', '\'', '"'])
                || (s.ends_with(')') && s.matches(')').count() > s.matches('(').count());
            if !trim || end <= start {
                break;
            }
            end -= 1;
        }
        if end > start + scheme {
            out.push(start..end);
        }
        from = end.max(start + 1);
    }
    out
}

/// Up to two initials from a nickname, uppercased: "Ada Lovelace" is "AL", "nova" is "N",
/// and "ßa" is "SS" (uppercasing can give more than one character, so cut after it).
pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .take(2)
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u32, author: u32, time: u32) -> Message {
        Message {
            id,
            key: id.to_string(),
            author,
            time,
            text: String::new(),
            source: String::new(),
            reply_to: None,
            reactions: vec![],
            edited: false,
            pending: None,
        }
    }

    fn sizes(m: &[Message]) -> Vec<usize> {
        groups(m).iter().map(|g| g.messages.len()).collect()
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
        assert_eq!(sizes(&m), [2, 1, 1, 1]);
        assert!(groups(&[]).is_empty());
    }

    #[test]
    fn groups_join_at_five_minutes_and_split_past_it() {
        assert_eq!(sizes(&[msg(1, 1, 600), msg(2, 1, 605)]), [2]);
        assert_eq!(sizes(&[msg(1, 1, 600), msg(2, 1, 606)]), [1, 1]);
    }

    #[test]
    fn groups_split_at_midnight_even_two_minutes_apart() {
        assert_eq!(sizes(&[msg(1, 1, 1439), msg(2, 1, 1441)]), [1, 1]);
    }

    #[test]
    fn initials_are_at_most_two_uppercased_characters() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("nova"), "N");
        assert_eq!(initials("  "), "");
        assert_eq!(initials("ada b c"), "AB");
        assert_eq!(initials("ßa"), "SS");
        assert_eq!(initials("ßa bob"), "SS");
    }

    #[test]
    fn times_and_day_labels() {
        assert_eq!(clock(14 * 60 + 5), "14:05");
        assert_eq!(clock(MINUTES_PER_DAY + 61), "01:01");
        let today = days_from_civil(2026, 11, 6);
        assert_eq!(civil_from_days(today), (2026, 11, 6));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(day_label(today, today), "Today");
        assert_eq!(day_label(today - 1, today), "Yesterday");
        assert_eq!(day_label(today - 2, today), "Wednesday, November 4");
        let last_year = days_from_civil(2025, 6, 3);
        assert_eq!(day_label(last_year, today), "Tuesday, June 3, 2025");
    }

    #[test]
    fn previews_are_one_line_and_cut_cleanly() {
        assert_eq!(preview("one\n\ntwo   three", 40), "one two three");
        assert_eq!(
            preview("Photos are up. Grandma looks happy", 16),
            "Photos are up…"
        );
        assert_eq!(preview("short", 5), "short");
        assert_eq!(preview("abcdefghij", 4), "abcd…");
        // The cut falls just before a space, so the last word is whole and stays.
        assert_eq!(preview("one two three", 7), "one two…");
        assert_eq!(preview("one two three", 6), "one…");
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
        // http:// is one character shorter than https://, so its shortest link is too.
        let t = "at http://a now";
        assert_eq!(
            links(t).into_iter().map(|r| &t[r]).collect::<Vec<_>>(),
            ["http://a"]
        );
        let t = "see https://en.wikipedia.org/wiki/Rust_(language).";
        assert_eq!(
            links(t).into_iter().map(|r| &t[r]).collect::<Vec<_>>(),
            ["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
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
            send_block: None,
            notices: Vec::new(),
        };
        let id = room.send("hi", 100);
        assert_eq!(id, 8);
        let last = room.last_message().unwrap();
        assert_eq!((last.author, last.time, last.text.as_str()), (1, 900, "hi"));
        room.messages[0].id = u32::MAX;
        assert_eq!(room.send("again", 900), u32::MAX);
    }
}
