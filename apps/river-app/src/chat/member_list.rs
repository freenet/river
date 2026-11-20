//! The members panel's order and its search box.
//!
//! **Order.** The web UI (`ui/src/components/members.rs`: `invite_tree_order`,
//! `deputy_display_order`, `pin_self_to_top`) lists the room as a display tree, flattened, with
//! no indentation:
//!
//! - the owner is the root, and every member comes after the member who invited them,
//!   depth first, siblings in River's own member order (`Member::order`);
//! - a deputy whose appointer matters to this viewer (the same set that draws the shield, which
//!   is what `Member::deputized_by` holds) moves under the appointer highest in the invite tree
//!   (the owner wins), carrying their own invitees, and is listed before that parent's regular
//!   invitees. A move that would make a member an ancestor of their new parent is skipped;
//! - a member whose inviter is no longer in the room, or whose invite chain never reaches the
//!   owner (a cycle), is listed after the tree, in River's member order;
//! - you are lifted to the top of the list, and everyone else keeps their place.
//!
//! The web UI draws no indent or marker for the tree, so the native list does not either: the
//! shield, "you" and owner marks are what the rows carry.
//!
//! **Search.** The box filters the list by nickname, live. Case and combining accents do not
//! matter, precomposed letters included (`visual_fold::search_fold`, built from the accent
//! step of the web UI's own fold); the web UI only lowercases. The skeleton folds are not used
//! because they merge look-alike letters (`l` and `I`), which would show unrelated members for
//! a plain query. The list keeps its order, so a match keeps its
//! tree place and you stay first.
//!
//! The box is per room and lives in memory only. Switching rooms closes and clears it, as the
//! web UI does: a query left over from another room would hide everyone in the next one. It has
//! no chord (Ctrl+F is the message search): the search button in the panel's header opens it
//! and closes it again.
//!
//! Only one search holds the keyboard. Opening the box closes the message search and Ctrl+F
//! closes the box; the composer gets the keyboard back from whichever was opened over it.
//! A per-frame hook ([`Chat::close_member_search_if_hidden`]) closes and clears the box when
//! its panel stops being drawn (toggled off, window resized between wide and narrow, room
//! changed), so a box nobody can see never holds the keyboard or takes an Escape.
//!
//! Escape: when the box's field has the keyboard, the first Escape closes and clears the box,
//! including in the narrow overlay (where a second Escape then closes the overlay). Otherwise
//! the box is closed after the message search and before the reply or edit.
//!
//! A query that is not blank but folds to nothing (a lone combining accent) matches no member.
//!
//! Ordering cost: depths are resolved once per call and the display-tree ancestor check walks
//! up the tree per deputy move, so a pathological chain with many deputies of deep members is
//! O(members x depth); real invite chains are short.
//!
//! Both are pure functions over the view model, tested without a window. What the panel draws
//! is cached until the members, the query or the room change (see [`Chat::member_rows`]), so a
//! frame that changed nothing sorts and filters nothing.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use rune::TextEdit;

use super::Chat;
use super::model::{Appointer, Member, MemberId, Role, Room};
use super::visual_fold::search_fold;

/// Indexes into `members`, in the order the panel lists them, for the viewer `me`.
pub fn display_order(members: &[Member], me: MemberId) -> Vec<usize> {
    let owner = members.iter().find(|m| m.role == Role::Owner).map(|m| m.id);
    let at: HashMap<MemberId, usize> = {
        let mut at = HashMap::new();
        for (i, m) in members.iter().enumerate() {
            at.entry(m.id).or_insert(i);
        }
        at
    };

    // River's member order; ties (example rooms, where every `order` is 0) keep list order.
    let mut by_river: Vec<usize> = (0..members.len()).collect();
    by_river.sort_by_key(|&i| (members[i].order, i));

    // The plain invite tree (the web UI's `invite_tree_order`).
    let base = invite_tree(owner, members, &by_river);
    let rank: HashMap<MemberId, usize> = base
        .iter()
        .enumerate()
        .map(|(r, &i)| (members[i].id, r))
        .collect();
    let depths = invite_depths(owner, members, &at);
    let depth_of = |id: MemberId| depths.get(&id).copied().unwrap_or(usize::MAX);

    // Re-parent deputies (the web UI's `deputy_display_order`). `None` is a parent that is not
    // in the room: the member is not reachable from the owner.
    let mut parent: HashMap<MemberId, Option<MemberId>> =
        members.iter().map(|m| (m.id, m.invited_by)).collect();
    let mut moved: HashSet<MemberId> = HashSet::new();
    for &i in &base {
        let m = &members[i];
        if Some(m.id) == owner {
            continue;
        }
        let chosen = m
            .deputized_by
            .iter()
            .filter_map(|a| match *a {
                Appointer::Owner => owner,
                // No viewer in the room (not yet listed): nothing is appointed by them.
                Appointer::You => at.contains_key(&me).then_some(me),
                // Someone no longer in the room appoints no one the viewer sees.
                Appointer::Member(id) => at.contains_key(&id).then_some(id),
            })
            .min_by_key(|&d| (depth_of(d), rank.get(&d).copied().unwrap_or(usize::MAX)));
        let Some(d) = chosen else { continue };
        if Some(d) == m.invited_by {
            // Deputized by their own inviter: no move, but still listed first.
            moved.insert(m.id);
        } else if !is_ancestor(m.id, d, owner, &parent, members.len()) {
            parent.insert(m.id, Some(d));
            moved.insert(m.id);
        }
        // Otherwise the move would make them their own ancestor: they stay under their inviter.
    }

    let mut children: HashMap<MemberId, Vec<usize>> = HashMap::new();
    for &i in &base {
        let m = &members[i];
        if Some(m.id) == owner {
            continue;
        }
        if let Some(Some(p)) = parent.get(&m.id) {
            children.entry(*p).or_default().push(i);
        }
    }
    for kids in children.values_mut() {
        // Deputies that moved first, then regular invitees; each group in tree order.
        kids.sort_by_key(|&c| (!moved.contains(&members[c].id), rank[&members[c].id]));
    }

    let mut out = Vec::with_capacity(members.len());
    let mut seen: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = owner
        .and_then(|o| at.get(&o).copied())
        .into_iter()
        .collect();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        out.push(i);
        if let Some(kids) = children.get(&members[i].id) {
            stack.extend(kids.iter().rev().copied());
        }
    }
    // Not reachable from the owner: after the tree, in the plain tree's order.
    out.extend(base.iter().copied().filter(|i| !seen.contains(i)));
    pin_self(&mut out, members, me);
    out
}

/// The members by invite tree, depth first from the owner, siblings in `by_river` order, then
/// whoever the owner does not reach. Every index appears once.
fn invite_tree(owner: Option<MemberId>, members: &[Member], by_river: &[usize]) -> Vec<usize> {
    let mut children: HashMap<MemberId, Vec<usize>> = HashMap::new();
    for &i in by_river {
        if Some(members[i].id) == owner {
            continue;
        }
        if let Some(by) = members[i].invited_by {
            children.entry(by).or_default().push(i);
        }
    }
    let at_owner = owner.and_then(|o| members.iter().position(|m| m.id == o));
    let mut out = Vec::with_capacity(members.len());
    let mut seen: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = at_owner.into_iter().collect();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        out.push(i);
        if let Some(kids) = children.get(&members[i].id) {
            stack.extend(kids.iter().rev().copied());
        }
    }
    out.extend(by_river.iter().copied().filter(|i| !seen.contains(i)));
    out
}

/// How many invites each member is below the owner (the owner is 0); `usize::MAX` when the
/// chain does not reach the owner or loops. Every member is resolved once (a chain is walked
/// until it meets a member already resolved), so this is linear however deep the chains are.
fn invite_depths(
    owner: Option<MemberId>,
    members: &[Member],
    at: &HashMap<MemberId, usize>,
) -> HashMap<MemberId, usize> {
    let mut memo: HashMap<MemberId, usize> = HashMap::with_capacity(members.len());
    if let Some(o) = owner {
        memo.insert(o, 0);
    }
    let mut on_path: HashSet<MemberId> = HashSet::new();
    let mut path: Vec<MemberId> = Vec::new();
    for m in members {
        let mut cur = m.id;
        let mut base;
        loop {
            if let Some(&v) = memo.get(&cur) {
                base = v;
                break;
            }
            if !on_path.insert(cur) {
                base = usize::MAX; // a loop
                break;
            }
            path.push(cur);
            match at.get(&cur).and_then(|&i| members[i].invited_by) {
                Some(next) => cur = next,
                None => {
                    base = usize::MAX; // the chain ends short of the owner
                    break;
                }
            }
        }
        for id in path.drain(..).rev() {
            on_path.remove(&id);
            base = if base == usize::MAX { base } else { base + 1 };
            memo.insert(id, base);
        }
    }
    memo
}

/// Whether `ancestor` is `node` or above it in the display tree built so far. `limit` (the
/// member count) bounds the walk, so a loop in `parent` ends it without a visited set.
fn is_ancestor(
    ancestor: MemberId,
    node: MemberId,
    owner: Option<MemberId>,
    parent: &HashMap<MemberId, Option<MemberId>>,
    limit: usize,
) -> bool {
    let mut cur = node;
    for _ in 0..=limit {
        if cur == ancestor {
            return true;
        }
        if Some(cur) == owner {
            return false;
        }
        match parent.get(&cur) {
            Some(Some(p)) => cur = *p,
            _ => return false,
        }
    }
    false
}

/// Lift `me` to the front, everyone else keeping their place. Nothing happens when `me` is not
/// listed (you joined and the list has not caught up).
fn pin_self(order: &mut [usize], members: &[Member], me: MemberId) {
    if let Some(pos) = order.iter().position(|&i| members[i].id == me) {
        order[..=pos].rotate_right(1);
    }
}

/// The members of `order` whose nickname contains `query` (trimmed, folded). A blank query
/// keeps everyone. `order` is kept.
pub fn filter(members: &[Member], order: &[usize], query: &str) -> Vec<usize> {
    // Blank (judged on what was typed, not on its fold) keeps everyone. A query that is not
    // blank but folds to nothing, such as a lone combining accent, matches no one: the web
    // UI's literal `contains` of that mark finds names that carry it, which no name shown here
    // does once its accents are dropped.
    if query.trim().is_empty() {
        return order.to_vec();
    }
    let needle = search_fold(query.trim());
    if needle.is_empty() {
        return Vec::new();
    }
    order
        .iter()
        .copied()
        .filter(|&i| search_fold(&members[i].nickname).contains(&needle))
        .collect()
}

/// The members box: whether it is open, what is typed, and the last result.
pub struct MemberSearch {
    pub open: bool,
    pub query: TextEdit,
    /// Whether the composer had the keyboard when the box opened, to give it back on close.
    restore_composer: bool,
    /// The last rows, with what they were computed for.
    cache: RefCell<Option<(u64, Rc<Vec<usize>>)>>,
    /// How many times the order and filter have run, for tests.
    pub runs: Cell<u32>,
}

impl Default for MemberSearch {
    fn default() -> Self {
        MemberSearch {
            open: false,
            query: TextEdit::new(""),
            restore_composer: false,
            cache: RefCell::new(None),
            runs: Cell::new(0),
        }
    }
}

/// What the rows depend on: the room, the query, and each member's id, name, inviter, role,
/// appointers and order. Hashed without sorting anything, so it is cheap enough every frame.
fn signature(query: &str, room: &Room) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut h);
    (&room.key, room.me).hash(&mut h);
    for m in &room.members {
        (m.id, &m.nickname, m.invited_by, m.role as u8, m.order).hash(&mut h);
        for a in &m.deputized_by {
            match *a {
                Appointer::Owner => 0u8.hash(&mut h),
                Appointer::You => 1u8.hash(&mut h),
                Appointer::Member(id) => (2u8, id).hash(&mut h),
            }
        }
        // Separates one member's appointers from the next member's fields.
        m.deputized_by.len().hash(&mut h);
    }
    h.finish()
}

impl Chat {
    /// The rows of the members panel for the open room: indexes into `Room::members`, in
    /// order, narrowed by the box. Kept until the members, the query or the room change.
    pub fn member_rows(&self) -> Rc<Vec<usize>> {
        let Some(room) = self.room() else {
            return Rc::default();
        };
        let s = &self.member_search;
        let query = if s.open { s.query.text.as_str() } else { "" };
        let sig = signature(query, room);
        if let Some((have, rows)) = &*s.cache.borrow()
            && *have == sig
        {
            return Rc::clone(rows);
        }
        s.runs.set(s.runs.get() + 1);
        let order = display_order(&room.members, room.me);
        let rows = Rc::new(filter(&room.members, &order, query));
        *s.cache.borrow_mut() = Some((sig, Rc::clone(&rows)));
        rows
    }

    /// Open the box (taking the keyboard from the composer, or from the message search, which
    /// it closes) or close it, clearing what was typed.
    pub fn toggle_member_search(&mut self) {
        if self.member_search.open {
            self.close_member_search();
            return;
        }
        // One search at a time: the message search gives way, and the composer gets the
        // keyboard back from whichever of them was opened over it.
        let from_search = self.search.take().is_some_and(|s| s.restore_composer);
        let had = std::mem::replace(&mut self.composer.focused, false) || from_search;
        let s = &mut self.member_search;
        s.open = true;
        s.restore_composer = had;
        s.query = TextEdit::new("");
        s.query.focused = true;
    }

    /// Close and clear the box without touching the composer; says whether the composer had
    /// the keyboard when the box opened.
    pub(super) fn dismiss_member_search(&mut self) -> bool {
        let s = &mut self.member_search;
        let restore = s.open && s.restore_composer;
        s.open = false;
        s.restore_composer = false;
        s.query = TextEdit::new("");
        restore
    }

    /// Close the box and clear it, giving the composer the keyboard back if it had it when the
    /// box opened (and the room can still be written in).
    pub fn close_member_search(&mut self) {
        if self.dismiss_member_search() && self.actionable().is_some() {
            self.composer.focused = true;
        }
    }

    /// The room changed: the box closes and clears, and nothing gets the keyboard back (the
    /// composer of the other room keeps its own focus state).
    pub fn drop_member_search(&mut self) {
        self.dismiss_member_search();
    }

    /// Whether the members panel is drawn in a window of the width last laid out.
    fn members_visible(&self) -> bool {
        self.members_shown(self.narrow)
    }

    /// Called every frame, the one place that keeps the box from outliving its panel: the panel
    /// can be hidden by its toggle, by the window changing between wide and narrow, or by
    /// opening another room, and an open box nobody can see would hold the keyboard flag and
    /// take the next Escape.
    ///
    /// Under a dialog (or the quick switcher) the box is dropped with nothing restored, as the
    /// message search is: the dialog's field keeps the keyboard, and the composer must not take
    /// it. The same applies when the panel is hidden while a dialog is up.
    pub fn close_member_search_if_hidden(&mut self) {
        if !self.member_search.open {
            return;
        }
        if self.modal_dialog_open() || self.switcher.is_some() {
            self.drop_member_search();
        } else if !self.members_visible() {
            self.close_member_search();
        }
    }

    /// Escape for the box. The box takes it first when its field has the keyboard, even over a
    /// narrow window's overlay (the overlay's own Escape then closes the panel on the next
    /// press); otherwise it is one of the last Escapes `Chat::escape` gives out. Says whether
    /// it closed the box.
    pub(super) fn member_search_escape(&mut self, field_focused_only: bool) -> bool {
        let s = &self.member_search;
        if !s.open || !self.members_visible() || (field_focused_only && !s.query.focused) {
            return false;
        }
        self.close_member_search();
        true
    }

    /// Clear the typed query and keep the box open and focused.
    pub fn clear_member_query(&mut self) {
        self.member_search.query = TextEdit::new("");
        self.member_search.query.focused = true;
    }

    /// Give the composer the keyboard on purpose (a reply started, an emoji picked): the
    /// members box, if open, no longer holds it.
    pub fn focus_composer(&mut self) {
        self.composer.focused = true;
        self.member_search.query.focused = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: MemberId = 99;

    fn owner() -> Member {
        Member::new(1, "Owner", Role::Owner)
    }

    fn m(id: MemberId, name: &str, by: MemberId) -> Member {
        let mut m = Member::new(id, name, Role::Member);
        m.invited_by = Some(by);
        m.order = i64::from(id);
        m
    }

    fn names(members: &[Member], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| members[i].nickname.clone()).collect()
    }

    #[test]
    fn invitees_follow_their_inviter_depth_first() {
        // Owner invited A(2) and B(3); A invited C(4); C invited D(5); B invited E(6).
        let members = vec![
            owner(),
            m(3, "B", 1),
            m(2, "A", 1),
            m(6, "E", 3),
            m(4, "C", 2),
            m(5, "D", 4),
        ];
        let order = display_order(&members, 1);
        assert_eq!(
            names(&members, &order),
            ["Owner", "A", "C", "D", "B", "E"],
            "siblings by River's order, each subtree before the next sibling"
        );
    }

    #[test]
    fn you_are_lifted_to_the_top_and_the_rest_keep_their_place() {
        let members = vec![owner(), m(2, "A", 1), m(3, "B", 2), m(4, "C", 1)];
        let order = display_order(&members, 3);
        assert_eq!(names(&members, &order), ["B", "Owner", "A", "C"]);
        // A viewer who is not listed changes nothing.
        let order = display_order(&members, ME);
        assert_eq!(names(&members, &order), ["Owner", "A", "B", "C"]);
    }

    #[test]
    fn a_missing_inviter_or_a_cycle_goes_after_the_tree_in_member_order() {
        let mut gone = m(7, "Orphan", 500);
        gone.invited_by = None;
        // 8 and 9 invited each other and were never reached from the owner.
        let members = vec![
            owner(),
            m(9, "Loop2", 8),
            gone,
            m(8, "Loop1", 9),
            m(2, "A", 1),
        ];
        let order = display_order(&members, ME);
        assert_eq!(
            names(&members, &order),
            ["Owner", "A", "Orphan", "Loop1", "Loop2"]
        );
        assert_eq!(order.len(), members.len());
    }

    #[test]
    fn a_deputy_of_the_owner_rises_to_the_top_with_their_invitees() {
        let mut dee = m(5, "Dee", 4);
        dee.deputized_by = vec![Appointer::Owner];
        // Owner invited A(2); A invited B(3) and C(4); C invited Dee(5); Dee invited F(6).
        let members = vec![
            owner(),
            m(2, "A", 1),
            m(3, "B", 2),
            m(4, "C", 2),
            dee,
            m(6, "F", 5),
            m(7, "G", 1),
        ];
        let order = display_order(&members, ME);
        assert_eq!(
            names(&members, &order),
            ["Owner", "Dee", "F", "A", "B", "C", "G"],
            "Dee and Dee's invitees move under the owner, before the owner's own invitees"
        );
    }

    #[test]
    fn a_deputy_of_a_viewer_ancestor_moves_under_the_one_highest_in_the_tree() {
        // A(2) is the viewer's ancestor; X(6) was invited by an unrelated branch.
        let mut x = m(6, "X", 3);
        x.deputized_by = vec![Appointer::Member(4), Appointer::Member(2)];
        let members = vec![
            owner(),
            m(2, "A", 1),
            m(3, "B", 1),
            m(4, "Under", 2),
            x,
            m(ME, "Me", 4),
        ];
        let order = display_order(&members, ME);
        // Me first; X sits under A (depth 1), not under Under (depth 2).
        assert_eq!(
            names(&members, &order),
            ["Me", "Owner", "A", "X", "Under", "B"]
        );
    }

    #[test]
    fn deputizing_your_own_inviter_keeps_the_place_but_lists_first() {
        let mut d = m(3, "D", 2);
        d.deputized_by = vec![Appointer::Member(2)];
        let members = vec![owner(), m(2, "A", 1), m(4, "Z", 2), d];
        let order = display_order(&members, ME);
        assert_eq!(names(&members, &order), ["Owner", "A", "D", "Z"]);
    }

    #[test]
    fn a_move_that_would_make_a_member_their_own_ancestor_is_skipped() {
        // Owner invited A(2); A invited B(3). A is B's deputy-appointer of record, and B is A's:
        // moving A under B would make B (A's invitee) A's parent and A B's.
        let mut a = m(2, "A", 1);
        a.deputized_by = vec![Appointer::Member(3)];
        let members = vec![owner(), a, m(3, "B", 2)];
        let order = display_order(&members, ME);
        assert_eq!(names(&members, &order), ["Owner", "A", "B"]);
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn a_deputy_you_appointed_rises_under_you() {
        let mut d = m(4, "D", 3);
        d.deputized_by = vec![Appointer::You];
        let members = vec![
            owner(),
            m(ME, "Me", 1),
            m(3, "Other", 1),
            d,
            m(5, "Mine", ME),
        ];
        let order = display_order(&members, ME);
        // The tree is Owner, Other, Me, D, Mine (siblings by River's order, and D moved under
        // Me); you are then lifted out to the front and the rest keep their places.
        assert_eq!(
            names(&members, &order),
            ["Me", "Owner", "Other", "D", "Mine"]
        );
    }

    #[test]
    fn example_rooms_with_equal_orders_keep_list_order() {
        let mut members = vec![owner(), m(3, "B", 1), m(2, "A", 1)];
        for x in &mut members {
            x.order = 0;
        }
        let order = display_order(&members, ME);
        assert_eq!(names(&members, &order), ["Owner", "B", "A"]);
    }

    #[test]
    fn a_room_with_no_owner_row_still_lists_everyone_once() {
        let members = vec![m(2, "A", 1), m(3, "B", 2)];
        let order = display_order(&members, ME);
        assert_eq!(names(&members, &order), ["A", "B"]);
    }

    #[test]
    fn a_large_room_is_ordered_in_one_pass() {
        // A chain 20 000 deep with a deputy at every level: nothing here may be quadratic or
        // recurse. (A time-free check: it simply has to finish and list everyone once.)
        let n: MemberId = 20_000;
        let mut members = vec![owner()];
        for id in 2..n {
            let mut x = m(id, &format!("M{id}"), id - 1);
            if id % 3 == 0 {
                x.deputized_by = vec![Appointer::Owner];
            }
            members.push(x);
        }
        let order = display_order(&members, MemberId::MAX);
        assert_eq!(order.len(), members.len());
        let unique: HashSet<usize> = order.iter().copied().collect();
        assert_eq!(unique.len(), members.len());
        assert_eq!(members[order[0]].nickname, "Owner");
    }

    #[test]
    fn a_query_of_only_a_combining_mark_matches_no_one_and_a_blank_one_matches_everyone() {
        let members = vec![owner(), m(2, "Ana", 1)];
        let order = display_order(&members, ME);
        assert!(filter(&members, &order, "\u{301}").is_empty());
        assert_eq!(filter(&members, &order, " \t "), order);
    }

    #[test]
    fn river_member_ids_sort_by_their_signed_number_which_is_what_order_holds() {
        // `rooms/room.rs` fills `Member::order` with `id.0.0`, and a room's member list is
        // sorted by `MemberId`, whose derived `Ord` compares that signed i64.
        use freenet_scaffold::util::FastHash;
        use river_core::room_state::member::MemberId as River;
        let ids = [i64::MIN, -9_000_000_000, -1, 0, 7, i64::MAX];
        let river: Vec<River> = ids.iter().map(|&n| River(FastHash(n))).collect();
        for (a, b) in river.iter().zip(river.iter().skip(1)) {
            assert!(a < b && a.0.0 < b.0.0);
        }
        // Siblings listed in that order whatever order the room hands them over in.
        let mut members = vec![owner()];
        for (k, &n) in ids.iter().enumerate().rev() {
            let mut x = m(10 + k as MemberId, &format!("S{k}"), 1);
            x.order = n;
            members.push(x);
        }
        let order = display_order(&members, ME);
        assert_eq!(
            names(&members, &order),
            ["Owner", "S0", "S1", "S2", "S3", "S4", "S5"]
        );
    }

    #[test]
    fn a_deep_chain_with_deputies_of_deep_members_is_ordered_with_each_member_once() {
        // 3 000 deep; every 5th member is a deputy of a member some way up the chain, so the
        // depth lookups and ancestor walks are exercised on deep chains. No timing is asserted:
        // it has to finish, list everyone once, and put the chain in order.
        let n: MemberId = 3_000;
        let mut members = vec![owner()];
        for id in 2..n {
            let mut x = m(id, &format!("M{id}"), id - 1);
            if id % 5 == 0 {
                x.deputized_by = vec![Appointer::Member(id - 3), Appointer::Member(id / 2)];
            }
            members.push(x);
        }
        let order = display_order(&members, MemberId::MAX);
        assert_eq!(order.len(), members.len());
        assert_eq!(
            order.iter().copied().collect::<HashSet<_>>().len(),
            members.len()
        );
        assert_eq!(members[order[0]].nickname, "Owner");
        // A plain deep chain keeps its order.
        let plain: Vec<Member> = std::iter::once(owner())
            .chain((2..n).map(|id| m(id, "x", id - 1)))
            .collect();
        let o = display_order(&plain, MemberId::MAX);
        assert_eq!(o, (0..plain.len()).collect::<Vec<_>>());
    }

    #[test]
    fn depths_are_resolved_once_for_chains_loops_and_orphans() {
        let mut orphan = m(7, "O", 500);
        orphan.invited_by = None;
        let members = vec![
            owner(),
            m(2, "A", 1),
            m(3, "B", 2),
            orphan,
            m(8, "L1", 9),
            m(9, "L2", 8),
            m(10, "T", 8),
        ];
        let at: HashMap<MemberId, usize> =
            members.iter().enumerate().map(|(i, x)| (x.id, i)).collect();
        let d = invite_depths(Some(1), &members, &at);
        assert_eq!((d[&1], d[&2], d[&3]), (0, 1, 2));
        assert_eq!(
            (d[&7], d[&8], d[&9], d[&10]),
            (usize::MAX, usize::MAX, usize::MAX, usize::MAX)
        );
    }

    #[test]
    fn an_appointment_by_a_viewer_who_is_not_listed_leaves_the_deputy_under_its_inviter() {
        let mut d = m(3, "D", 2);
        d.deputized_by = vec![Appointer::You];
        let members = vec![owner(), m(2, "A", 1), m(4, "Z", 2), d];
        let order = display_order(&members, ME);
        assert_eq!(names(&members, &order), ["Owner", "A", "D", "Z"]);
    }

    #[test]
    fn an_appointer_who_left_the_room_moves_no_one() {
        let mut x = m(3, "X", 2);
        x.deputized_by = vec![Appointer::Member(777)];
        let members = vec![owner(), m(2, "A", 1), x, m(4, "B", 1)];
        let order = display_order(&members, ME);
        assert_eq!(names(&members, &order), ["Owner", "A", "X", "B"]);
    }

    #[test]
    fn the_filter_ignores_case_and_accents_and_keeps_the_order() {
        let members = vec![
            owner(),
            m(2, "José Núñez", 1),
            m(3, "Jose Other", 1),
            m(4, "Cafe\u{301} Ana", 1),
            m(5, "JOSEPH", 1),
        ];
        let order = display_order(&members, ME);
        let got = filter(&members, &order, "  jose ");
        assert_eq!(
            names(&members, &got),
            ["José Núñez", "Jose Other", "JOSEPH"]
        );
        let got = filter(&members, &order, "nunez");
        assert_eq!(names(&members, &got), ["José Núñez"]);
        let got = filter(&members, &order, "jose");
        assert_eq!(names(&members, &got).len(), 3);
        // Look-alike letters stay apart: a capital I is not an l.
        let il = vec![m(2, "Ilya", 1), m(3, "llya", 1)];
        let o = vec![0, 1];
        assert_eq!(filter(&il, &o, "ilya"), vec![0]);
        let got = filter(&members, &order, "CAFE");
        assert_eq!(names(&members, &got), ["Cafe\u{301} Ana"]);
        assert_eq!(
            filter(&members, &order, "   "),
            order,
            "blank keeps everyone"
        );
        assert!(filter(&members, &order, "zzz").is_empty());
    }
}

#[cfg(test)]
mod ui_tests {
    //! The box driven as a person would: pressing the button, typing, picking a member.

    use rune::{Env, Key, KeyEvent, Runtime};

    use crate::{RiverApp, Setup, WINDOW_SIZE, view};

    fn render() -> Runtime<RiverApp> {
        let mut s = RiverApp::detached();
        s.setup = Setup::Connected {
            version: "0.2.155".into(),
        };
        let env = Env {
            system_dark: true,
            scale: 1.0,
        };
        let mut rt = Runtime::new(s, view, WINDOW_SIZE, env);
        rt.on_key = crate::spec().on_key;
        // The per-frame hook that closes the box under a hidden panel runs from here.
        rt.poll = crate::spec().poll;
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        rt
    }

    fn settle(rt: &mut Runtime<RiverApp>) {
        for i in 0..6 {
            rt.frame(1.0 + f64::from(i) / 60.0);
        }
    }

    fn narrow() -> Runtime<RiverApp> {
        let mut rt = render();
        rt.set_viewport(crate::NARROW_SCENE_SIZE, 1.0);
        settle(&mut rt);
        rt
    }

    fn press(rt: &mut Runtime<RiverApp>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.1);
        rt.frame(0.2);
    }

    fn key(rt: &mut Runtime<RiverApp>, key: Key) {
        rt.key(KeyEvent {
            key,
            mods: Default::default(),
        });
        rt.frame(0.3);
    }

    fn type_text(rt: &mut Runtime<RiverApp>, text: &str) {
        for c in text.chars() {
            key(rt, Key::Text(c.to_string()));
        }
    }

    fn listed(rt: &Runtime<RiverApp>) -> Vec<String> {
        rt.elements()
            .into_iter()
            .filter_map(|e| e.key)
            .filter(|k| k.starts_with("member-") && k[7..].chars().all(|c| c.is_ascii_digit()))
            .collect()
    }

    fn box_focused(rt: &Runtime<RiverApp>) -> bool {
        rt.state.chat.member_search.query.focused
    }

    fn composer_focused(rt: &Runtime<RiverApp>) -> bool {
        rt.state.chat.composer.focused
    }

    #[test]
    fn typing_in_the_box_filters_the_list_and_a_row_opens_that_member() {
        let mut rt = render();
        assert!(
            rt.find("member-search-input").is_none(),
            "closed until asked"
        );
        let all = listed(&rt);
        assert_eq!(all.len(), 7);
        // You first, then the owner, then the invite tree (Jonas and his invitees, Priya's).
        assert_eq!(all[0], "member-21");
        assert_eq!(all[1], "member-12");

        press(&mut rt, "member-search-toggle");
        assert!(box_focused(&rt));
        type_text(&mut rt, "PRI");
        assert_eq!(rt.state.chat.member_search.query.text, "PRI");
        assert_eq!(listed(&rt), ["member-27"], "case does not matter");

        press(&mut rt, "member-27");
        assert_eq!(rt.state.chat.member_modal.as_deref(), Some("27"));
    }

    #[test]
    fn no_matches_says_so_and_the_clear_button_brings_everyone_back() {
        let mut rt = render();
        press(&mut rt, "member-search-toggle");
        assert!(
            rt.find("member-search-clear").is_none(),
            "nothing to clear yet"
        );
        type_text(&mut rt, "zzz");
        assert!(listed(&rt).is_empty());
        assert!(rt.find("member-search-empty").is_some());
        press(&mut rt, "member-search-clear");
        assert_eq!(rt.state.chat.member_search.query.text, "");
        assert_eq!(listed(&rt).len(), 7);
        assert!(rt.find("member-search-empty").is_none());
        assert!(rt.state.chat.member_search.open, "still open after a clear");
    }

    #[test]
    fn opening_the_box_takes_the_keyboard_and_escape_gives_it_back_to_the_composer() {
        let mut rt = render();
        rt.update(|s, _| s.chat.composer.focused = true);
        rt.frame(0.1);
        assert!(composer_focused(&rt));
        rt.update(|s, _| s.chat.toggle_member_search());
        rt.frame(0.2);
        assert!(!composer_focused(&rt), "typed text goes to one field only");
        type_text(&mut rt, "an");
        assert_eq!(
            rt.state.chat.composer.text, "",
            "nothing leaked into the composer"
        );
        key(&mut rt, Key::Escape);
        assert!(!rt.state.chat.member_search.open);
        assert_eq!(
            rt.state.chat.member_search.query.text, "",
            "closing clears it"
        );
        assert!(composer_focused(&rt));
        assert_eq!(listed(&rt).len(), 7);
    }

    #[test]
    fn the_box_never_takes_the_composers_focus_by_itself() {
        let mut rt = render();
        press(&mut rt, "composer-field");
        // Messages arriving, frames passing and the panel redrawing change nothing.
        for i in 0..5 {
            rt.frame(1.0 + f64::from(i));
        }
        assert!(composer_focused(&rt));
        assert!(!rt.state.chat.member_search.open);
        type_text(&mut rt, "an");
        assert_eq!(rt.state.chat.composer.text, "an");
    }

    #[test]
    fn the_message_search_chord_does_not_open_the_members_box() {
        let mut rt = render();
        rt.key(KeyEvent {
            key: Key::Command('f'),
            mods: rune::Mods {
                cmd: true,
                ctrl: true,
                ..Default::default()
            },
        });
        rt.frame(0.3);
        assert!(
            rt.state.chat.search.is_some(),
            "Ctrl+F is the message search"
        );
        assert!(!rt.state.chat.member_search.open);
    }

    #[test]
    fn switching_rooms_closes_and_clears_the_box() {
        let mut rt = render();
        press(&mut rt, "member-search-toggle");
        type_text(&mut rt, "an");
        let other = rt
            .state
            .chat
            .rooms()
            .iter()
            .map(|r| r.key.clone())
            .find(|k| k != "town-square")
            .expect("another example room");
        rt.update(|s, _| s.chat.select(&other));
        rt.frame(0.5);
        assert!(!rt.state.chat.member_search.open);
        assert_eq!(rt.state.chat.member_search.query.text, "");
        // And back: the first room lists everyone again.
        rt.update(|s, _| s.chat.select("town-square"));
        rt.frame(0.6);
        assert_eq!(listed(&rt).len(), 7);
    }

    #[test]
    fn a_frame_that_changed_nothing_does_not_sort_again_and_an_edit_does() {
        let mut rt = render();
        press(&mut rt, "member-search-toggle");
        type_text(&mut rt, "a");
        let runs = rt.state.chat.member_search.runs.get();
        for i in 0..10 {
            rt.frame(2.0 + f64::from(i));
        }
        assert_eq!(rt.state.chat.member_search.runs.get(), runs);
        type_text(&mut rt, "n");
        assert!(rt.state.chat.member_search.runs.get() > runs);
    }

    #[test]
    fn hiding_the_panel_with_the_box_open_closes_it_and_returns_the_keyboard() {
        let mut rt = render();
        rt.update(|s, _| s.chat.composer.focused = true);
        rt.frame(0.1);
        rt.update(|s, _| s.chat.toggle_member_search());
        type_text(&mut rt, "an");
        press(&mut rt, "toggle-members");
        settle(&mut rt);
        assert!(rt.find("members").is_none());
        let s = &rt.state.chat.member_search;
        assert!(!s.open && !s.query.focused && s.query.text.is_empty());
        assert!(composer_focused(&rt), "the composer has the keyboard again");
    }

    #[test]
    fn narrowing_the_window_with_the_box_open_closes_it() {
        let mut rt = render();
        rt.update(|s, _| s.chat.composer.focused = true);
        rt.frame(0.1);
        rt.update(|s, _| s.chat.toggle_member_search());
        rt.frame(0.2);
        assert!(rt.state.chat.member_search.open);
        rt.set_viewport(crate::NARROW_SCENE_SIZE, 1.0);
        settle(&mut rt);
        assert!(!rt.state.chat.member_search.open);
        assert!(composer_focused(&rt));
    }

    #[test]
    fn escape_after_the_panel_hid_cancels_a_reply_instead_of_vanishing_into_the_box() {
        let mut rt = render();
        let target = rt.state.chat.room().expect("room").messages[0].key.clone();
        rt.update(|s, _| s.chat.start_reply(&target));
        rt.frame(0.1);
        rt.update(|s, _| s.chat.toggle_member_search());
        rt.frame(0.2);
        press(&mut rt, "toggle-members");
        settle(&mut rt);
        assert!(matches!(rt.state.chat.mode, super::super::Mode::Reply(_)));
        key(&mut rt, Key::Escape);
        assert!(!matches!(rt.state.chat.mode, super::super::Mode::Reply(_)));
        // Even before a frame has run the hook, a hidden box takes no Escape.
        let mut chat = rt.state.chat;
        chat.start_reply(&target);
        chat.toggle_member_search();
        assert!(chat.escape(), "acts on the reply");
        assert!(!matches!(chat.mode, super::super::Mode::Reply(_)));
    }

    #[test]
    fn in_the_narrow_overlay_the_first_escape_closes_the_box_and_the_second_the_overlay() {
        let mut rt = narrow();
        press(&mut rt, "toggle-members");
        assert!(rt.find("members-overlay").is_some());
        press(&mut rt, "member-search-toggle");
        type_text(&mut rt, "an");
        key(&mut rt, Key::Escape);
        assert!(!rt.state.chat.member_search.open);
        assert_eq!(rt.state.chat.member_search.query.text, "");
        assert!(rt.find("members-overlay").is_some(), "still in the overlay");
        key(&mut rt, Key::Escape);
        settle(&mut rt);
        assert!(rt.find("members-overlay").is_none());
    }

    #[test]
    fn the_two_searches_never_hold_the_keyboard_together() {
        let mut rt = render();
        rt.update(|s, _| s.chat.composer.focused = true);
        rt.frame(0.1);
        rt.update(|s, _| s.chat.toggle_member_search());
        rt.frame(0.2);
        // Ctrl+F while the members box has the keyboard.
        rt.key(KeyEvent {
            key: Key::Command('f'),
            mods: rune::Mods {
                cmd: true,
                ctrl: true,
                ..Default::default()
            },
        });
        rt.frame(0.3);
        assert!(
            rt.state
                .chat
                .search
                .as_ref()
                .is_some_and(|s| s.query.focused)
        );
        assert!(!rt.state.chat.member_search.open);
        assert!(!rt.state.chat.member_search.query.focused);
        // The reverse: the members box opens over the message search.
        rt.update(|s, _| s.chat.toggle_member_search());
        rt.frame(0.4);
        assert!(rt.state.chat.search.is_none());
        assert!(rt.state.chat.member_search.query.focused);
        // Closing it gives the composer the keyboard it had before either opened.
        key(&mut rt, Key::Escape);
        assert!(composer_focused(&rt));
        assert!(!rt.state.chat.member_search.query.focused);
    }

    #[test]
    fn a_deliberate_composer_refocus_takes_the_flag_from_the_members_box() {
        let mut rt = render();
        let target = rt.state.chat.room().expect("room").messages[0].key.clone();
        rt.update(|s, _| s.chat.toggle_member_search());
        rt.frame(0.1);
        assert!(rt.state.chat.member_search.query.focused);
        rt.update(|s, _| s.chat.start_reply(&target));
        assert!(composer_focused(&rt));
        assert!(!rt.state.chat.member_search.query.focused);
    }

    #[test]
    fn a_row_picked_after_the_filter_changed_opens_that_member() {
        let mut rt = render();
        press(&mut rt, "member-search-toggle");
        type_text(&mut rt, "e");
        let wide = listed(&rt).len();
        assert!(wide > 1);
        type_text(&mut rt, "l");
        assert_eq!(listed(&rt), ["member-16"], "Elena only");
        press(&mut rt, "member-16");
        assert_eq!(rt.state.chat.member_modal.as_deref(), Some("16"));
        rt.update(|s, _| s.chat.member_modal = None);
        rt.update(|s, _| s.chat.clear_member_query());
        rt.frame(0.5);
        assert_eq!(listed(&rt).len(), 7);
    }

    #[test]
    fn a_dialog_opening_over_the_box_drops_it_and_the_composer_never_takes_the_keyboard() {
        let mut rt = render();
        rt.update(|s, _| s.chat.composer.focused = true);
        rt.frame(0.1);
        rt.update(|s, _| s.chat.toggle_member_search());
        rt.frame(0.2);
        rt.update(|s, _| s.chat.open_member("27"));
        settle(&mut rt);
        let s = &rt.state.chat.member_search;
        assert!(!s.open && !s.query.focused);
        assert!(!composer_focused(&rt), "nothing restored under a dialog");
        // A dialog field has the keyboard; hiding the panel or narrowing does not change that.
        rt.update(|s, _| s.chat.nickname.focused = true);
        press(&mut rt, "toggle-members");
        rt.set_viewport(crate::NARROW_SCENE_SIZE, 1.0);
        settle(&mut rt);
        assert!(!composer_focused(&rt));
        assert!(
            rt.state.chat.nickname.focused,
            "the dialog's field keeps it"
        );
    }

    #[test]
    fn hiding_the_panel_while_a_dialog_is_up_restores_nothing() {
        let mut chat = rt_chat();
        chat.composer.focused = true;
        chat.toggle_member_search();
        // Open a dialog without the per-frame hook having run, then hide the panel.
        chat.member_modal = Some("27".into());
        chat.toggle_members(false);
        chat.close_member_search_if_hidden();
        assert!(!chat.member_search.open);
        assert!(!chat.composer.focused);
    }

    fn rt_chat() -> super::super::Chat {
        render().state.chat
    }
}
