---
description: Reader-position policy for room history and DM threads
globs:
  - ui/src/components/conversation.rs
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

1. **The conversation is in the foreground** (`foreground::in_foreground`):
   the tab is visible, the history has layout (on mobile, the chat panel is
   the one showing), and no modal covers it. For the room that means no
   modal at all, the DM thread included; for a DM thread, no modal other than
   the thread itself. Popovers attached to a history row (the message action
   menu, the reaction picker) count as modals; composer popovers do not,
   because the composer does not scroll.
2. **The reader is at the end or near it:** before the patch, the newest
   item's bottom (`bottom-sentinel` / `dm-bottom-sentinel`, the same edge as
   Latest and the read rule, not the scroller's absolute end) sits at most
   `ROOM_FOLLOW_BAND_PX` (100) or `DM_FOLLOW_BAND_PX` (50) below the view's
   bottom edge. Those are the pre-#753 values (`BOTTOM_THRESHOLD_PX` and
   `is_near_bottom(.., 50.0)` at `8cf54dd7`), which measured from the
   absolute end.

**Only an arrival follows.** A tab becoming visible, a modal closing, or a
mobile panel revealing the chat never scrolls: messages that arrived in the
meantime stay below, Latest appears, and they stay unread until the reader
reaches them.

While another mobile panel (Rooms or Members) replaces the chat, Latest is
hidden with it, so the open room's unread count, under its notification mode,
shows on that panel's back-to-chat button (`back_to_chat.rs`). No other badge
changes: the hamburger and the open room's row in the room list still leave
that room out.

The check is **stateless and measured before the patch.** It reads the DOM in
the render that brings the arrival, while the DOM still shows the previous
render (`follow_arrival_at_end`, `dm_arrival_follows`). Nothing is remembered
between arrivals, so no flag can latch:

- [#486](https://github.com/freenet/river/issues/486): the old follow gate
  latched closed after the composer grew or the gap passed 100px;
- [#508](https://github.com/freenet/river/issues/508): a stale pin plus the
  max-scroll clamp yanked a reader;
- [#723](https://github.com/freenet/river/issues/723): an arrival before the
  reader's settle never re-armed the pin.

Measuring after the patch would count the arrival's own height against the
band, which is why the pre-#753 DM check missed tall inbound DMs.

An arrival is a strictly newer newest message: `NewestKey` (the sender's
unclamped time, then id: display order) in the room, `DmKey` (timestamp, then
purge token) in a thread. An edit, a deletion of the newest, a message
inserted above it, and a join folding into the trailing event summary (which
raises the key) are told apart by it. The 60s clock-skew clamp only affects
grouping and the timestamp shown.

**`ModalPresence` rule.** Every modal root, and any popover attached to a
history row, must mount a `foreground::ModalPresence`. It registers the modal
while mounted and bumps `FOREGROUND_CHANGED` when it unmounts. A modal without
one lets arrivals scroll, and the history be read, behind it.
`every_modal_root_has_a_presence` enforces it.

Accepted trade-offs:

- A reader dragging within the band when an arrival lands is taken to the
  end. The pre-#753 code fought this with `reader_moved_up_since`, the latch
  behind #508.
- The history follows, and is read, under composer popovers.
- A same-millisecond (room) or same-second (DM) arrival whose id or token
  sorts lower than the previous newest is not followed.
- A room message stamped far ahead stays the last row until real time passes
  it, and messages posted meanwhile land above it and are not followed.

## Room history

- Opening a room, an own send, Latest, and a followed arrival request one
  instant move to the end. A pending request carries its room so a room
  change cannot apply it to the next room. It waits for rows and visible
  layout when necessary. A request whose render would hold the range's end at
  the ceiling takes the latest range instead.
- After that request lands, an end hold keeps the end visible while rows
  change height, such as images loading or private messages decrypting. A
  change of newest message ends the hold, including deleting the newest; a
  followed arrival lands a new request, which holds again. A backfill, a
  message inserted above the newest, a trim and rows changing height keep
  it. Scrolling upward more than 4px from the scroller's absolute end, hiding
  the panel, changing rooms or an empty range also releases it. Accepted for
  now: padding below the newest message lets a small upward scroll release
  the hold while the message is still visible and Latest stays hidden.
- Outside that hold, a change in chat-area height preserves the bottom edge
  of the view. Width-only reflow gets no correction. Paging and panel reveals
  retain their reading-position corrections; deleting the reading row uses
  the row above as the preferred fallback.
- Latest appears when the newest message's bottom is more than 4px below
  the visible area, or a held range withholds newer messages. The margin is
  measured against the message, not the padding below it.
- A room's read marker advances when the history is in the foreground (same
  predicate as following), has completed its reveal restore, the rendered
  range reaches the room's latest message, and that message's bottom is on
  screen within the same 4px margin. It re-checks on `FOREGROUND_CHANGED`
  (the tab became visible or a modal closed), without scrolling. Scrolling
  through a partial range does not advance it. Every mark-read path is
  bounded by the message published in `NEWEST_SEEN`.

Browser scroll anchoring remains disabled on the room scroller. Custom
position corrections remain in place; the two known
[#507](https://github.com/freenet/river/issues/507) cases for late images and
bulk removal above a parked reader remain follow-up work.

## DM threads

DM threads share the policy: opening, own send and Latest jump instantly,
and an inbound DM follows a reader at the end by the rule above, with the
50px band. A thread opened empty has no opening to place, so its first DMs
follow by the same rule (in a hidden tab they leave it at the top). Latest
uses the same 4px message-bottom margin. The thread updates its read cutoff
to the newest rendered inbound DM timestamp only when it is in the foreground
and the newest DM's bottom is on screen, and re-checks on
`FOREGROUND_CHANGED`.

A thread's opening, own-send and follow placements run in a task queued after
the render, and both they and the read witness find the thread through the
global `dm-scroll-container` and `dm-bottom-sentinel` ids. Each mounted thread
body therefore owns a lifetime flag, set false synchronously when it
unmounts; a queued placement checks it as it runs, before any DOM lookup, and
the witness checks it before measuring. Work from a closed thread must not
scroll, or count as seen, whichever thread is open by then, including a
reopened instance of the same `(room, peer)`, which gets a fresh flag. A read
the witness already granted may still commit after a close. Pinned by
`ui/tests/dm-thread-lifecycle.spec.ts`.

This shared policy does not imply identical position correction machinery.
DM threads do not currently have the room history's end hold or explicit
composer/keyboard bottom-edge correction. Their timestamp cutoff and its
limitations are described in [direct-messages.md](direct-messages.md).
