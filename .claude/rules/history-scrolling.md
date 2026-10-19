---
description: Reader-position policy for room history and DM threads
globs:
  - ui/src/components/conversation.rs
  - ui/src/components/conversation/history_scroll.rs
  - ui/src/components/conversation/history_window.rs
  - ui/src/components/scroll_to_latest.rs
  - ui/src/components/foreground.rs
  - ui/src/components/app/document_title.rs
  - ui/src/components/direct_messages/dm_thread_modal.rs
  - ui/tests/*scroll*.spec.ts
  - ui/tests/conversation-history-position.spec.ts
  - ui/tests/room-unread-badge.spec.ts
---

# History scrolling

## Following a reader at the end

An arrival scrolls a room or a DM thread to its newest message only when
both of these hold at the moment it arrives:

1. **The history has layout** (on mobile, the chat panel is the one
   showing). The tab need not be visible, and a DM thread open over the room
   does not stop the room following: a reader who left the end in view (a
   background tab, an occluded or minimized window, a phone with its screen
   off, a DM thread) comes back to the end. Gating this on the foreground, as
   #753 did, parked such a reader above every arrival, and once the newest
   message sat past the band no later arrival followed either, so the history
   looked as if it had stopped following.
2. **The reader is at the end or near it:** before the patch, the newest
   item's bottom (`bottom-sentinel` / `dm-bottom-sentinel`, the same edge as
   Latest and the read rule, not the scroller's absolute end) sits at most
   `FOLLOW_BAND_PX` (100) below the view's bottom edge, in a room and in a DM
   thread alike.

Following never marks anything read. The read rule below still requires the
foreground (`foreground::in_foreground`: the tab visible, the history with
layout, and for the room no DM thread open), so arrivals followed while away
stay unread until the reader is back and they are on screen.

**Only an arrival follows.** A tab becoming visible, a DM thread closing, or a
mobile panel revealing the chat never scrolls. Messages that arrived while a
mobile panel hid the chat stay below, Latest appears, and they stay unread
until the reader reaches them.

While another mobile panel replaces the chat, Latest is hidden with it, so
the open room's unread count, under its notification mode, shows in one place
per panel. Neither marks anything read, and both count with
`count_unread_in_room_data_with_mode` under the room's mode, so Muted shows
nothing and Mentions & replies counts only arrivals that mention or reply to
the reader.

- **Rooms:** the open room's row in the room list shows its count as every
  other row does (`room_list.rs`, while `MOBILE_VIEW` is not `Chat`;
  `md:hidden`, since on desktop the chat always shows). That panel's back
  arrow has no badge.
- **Members:** the room list is hidden too, so the panel's back arrow shows
  the count (`members-current-room-unread-badge`, opted into through
  `BackToChatButton`'s `current_room_badge`), and its accessible label and
  tooltip word it as the room's row does ("Back to chat, 2 unread messages",
  or "…mentions or replies").

While the chat shows, the open room's row has no badge, and the hamburger,
which shows only with the chat, always leaves the open room out.

The check is **stateless and measured before the patch.** It reads the DOM in
the render that brings the arrival, while the DOM still shows the previous
render (`follow_arrival_at_end`, `HistoryDom::arrival_follows`). Nothing is remembered
between arrivals, so no flag can latch:

- [#486](https://github.com/freenet/river/issues/486): the old follow gate
  latched closed after the composer grew or the gap passed 100px;
- [#508](https://github.com/freenet/river/issues/508): a stale pin plus the
  max-scroll clamp yanked a reader;
- [#723](https://github.com/freenet/river/issues/723): an arrival before the
  reader's settle never re-armed the pin.

Measuring after the patch would count the arrival's own height against the
band, so a tall arrival would never follow.

An arrival is a strictly newer newest message: `NewestKey` (the sender's
unclamped time, then id: display order) in the room, `DmKey` (timestamp, then
purge token) in a thread. An edit, a deletion of the newest, a message
inserted above it, and a join folding into the trailing event summary (which
raises the key) are told apart by it. The 60s clock-skew clamp only affects
grouping and the timestamp shown.

Current behavior:

- A reader dragging within the band when an arrival lands is taken to the
  end. There is no gesture guard: remembering that the reader had moved up
  (`reader_moved_up_since`) is the latch that caused #508.
- The history follows under every dialog and popover, a DM thread over the
  room included, and is read under all of them except that DM thread: member
  info, room settings, invite and ban dialogs, the message action menu and
  the reaction picker. The reader is at the end anyway when the dialog
  closes, but those arrivals count as read without having been on screen
  unobstructed.

Known limitations:

- A same-millisecond (room) or same-second (DM) arrival whose id or token
  sorts lower than the previous newest is not followed
  ([#764](https://github.com/freenet/river/issues/764)).
- A room message stamped far ahead stays the last row until real time passes
  it, and messages posted meanwhile land above it and are not followed
  ([#70](https://github.com/freenet/river/issues/70), appending inbound messages regardless of timestamp).

## Room history

- Opening a room, an own send, Latest, and a followed arrival request one
  instant move to the end. A pending request carries its room so a room
  change cannot apply it to the next room. It waits for rows and visible
  layout when necessary. A request whose render would hold the range's end at
  the ceiling takes the latest range instead.
- After that request lands, an end hold keeps the end visible while the
  history or the chat area changes height: images loading, private messages
  decrypting, the composer growing, the phone keyboard opening. A scroll that
  settles within 4px of the scroller's absolute end, with the range reaching
  the latest message, arms the same hold, so a reader who scrolls back to the
  end by hand is held too. A change of newest message ends the hold,
  including deleting the newest, so an arrival follows only by the rule
  above; a followed arrival lands a new request, which holds again. A
  backfill, a message inserted above the newest, a trim and rows changing
  height keep it. Hiding the panel, changing rooms and an empty range release
  it, and so does a scroll the reader makes upward: one that raises both of
  the view's edges, the top edge more than 4px above where the view sits at
  the end, and ends more than 4px from the scroller's absolute end. Where the view
  sits at the end is taken afresh on every scroll event that keeps the hold
  (`end_hold_after_scroll`), so a reader leaving in steps of a pixel or two
  releases it as one larger scroll would, and a clamp or trim counts from the
  new end. The top edge is the reader's own move: content growing below the
  view, or the chat area shrinking, in the same frame as a scroll adds to the
  distance from the end but releases nobody inside the margin. A clamp from
  the chat area growing (the composer collapsing, the keyboard closing)
  raises only the top edge and keeps it, even when a row grows in the same
  frame. Known limitations: padding below the newest message
  lets a small upward scroll release the hold while the message is still
  visible and Latest stays hidden ([#765](https://github.com/freenet/river/issues/765)); a reader who scrolls up in
  the same frame the chat area grows by at least as much keeps the hold for
  that frame and is taken back to the end once, and their next scroll
  releases it ([#766](https://github.com/freenet/river/issues/766)).
- Outside that hold, a size change gets no correction: when the composer
  grows or the phone keyboard opens, the browser keeps the view's top edge,
  so the bottom of the view is covered. Paging and panel reveals retain their
  reading-position corrections; deleting the reading row uses the row above
  as the preferred fallback. The reading row is the first row whose top lies
  inside the view, else the row the view's top cuts through (a message
  taller than the view, not the row starting below the fold).
- Latest appears when the newest message's bottom is more than 4px below
  the visible area, or a held range withholds newer messages. The margin is
  measured against the message, not the padding below it.
- A room's read marker advances when the history is in the foreground (same
  predicate as following), has completed its reveal restore, the rendered
  range reaches the room's latest message, and that message's bottom is on
  screen within the same 4px margin. It re-checks when the tab becomes
  visible (`DOCUMENT_VISIBLE`) or a DM thread closes (`OPEN_DM_THREAD`),
  without scrolling. Scrolling
  through a partial range does not advance it. Every mark-read path is
  bounded by the message published in `NEWEST_SEEN`.

Browser scroll anchoring is disabled on the room scroller, and a browser test
asserts the computed `overflow-anchor: none`. Custom position corrections
remain in place. Known limitation: late images and bulk removal above a parked
reader are not corrected ([#507](https://github.com/freenet/river/issues/507)).

## DM threads

DM threads share the policy: opening, own send and Latest jump instantly,
an own send being one to this thread (a send completing in another thread,
counted per `(room, peer)`, never moves it), and an inbound DM follows a reader at the end by the rule above, with the
same band. A thread opened empty has no opening to place, so its first DMs
follow by the same rule, in a hidden tab too. Latest
uses the same 4px message-bottom margin. The thread updates its read cutoff
to the newest rendered inbound DM timestamp only when it is in the foreground
and the newest DM's bottom is on screen, and re-checks when the tab becomes
visible.

A thread's opening, own send and followed arrival each set a pending request
to go to the end, in render, and that render queues an effect
(`queue_effect`) that completes it with an instant jump once the render is in
the DOM, like the room's `complete_scroll_request`. Dioxus drops a removed
scope's queued effects, so no placement can outlive its thread. An own send's
plaintext is cached in the same `defer` block that adds it to `ROOMS`
(`save_outbound_dm_now`), so the jump lands on the sent bubble's final
height. The placement and the read witness find the thread through the
global `dm-scroll-container` and `dm-bottom-sentinel` ids, and every caller
of the witness runs while its thread is mounted: its effects (whose queued
runs Dioxus drops with the thread), the Latest click, and the sentinel
observer, which is disconnected with the thread. A read the witness already
granted may still commit after a close.

This shared policy does not imply identical position correction machinery.
Known limitations: DM threads do not have the room history's end hold, so a
composer or keyboard resize gets no correction there at all
([#767](https://github.com/freenet/river/issues/767)); and the read cutoff is a whole second, so DMs from one peer
in the same second are read together, even if one arrived later or is still
below the fold ([#764](https://github.com/freenet/river/issues/764)). The thread's read rule is described in
[direct-messages.md](direct-messages.md).
