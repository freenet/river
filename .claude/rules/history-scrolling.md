---
description: Reader-position policy for room history and DM threads
globs:
  - ui/src/components/conversation.rs
  - ui/src/components/scroll_to_latest.rs
  - ui/src/components/app/document_title.rs
  - ui/src/components/direct_messages/dm_thread_modal.rs
  - ui/tests/*scroll*.spec.ts
  - ui/tests/conversation-history-position.spec.ts
  - ui/tests/room-unread-badge.spec.ts
---

# History scrolling

## Why automatic following was removed

[PR #753](https://github.com/freenet/river/pull/753) deliberately replaces
automatic following with explicit requests to reach the latest message.
Incoming messages preserve the reader's place, including when the reader is
at the end. This gives arrivals one consistent rule and removes the follow
pin and its bookkeeping around programmatic scrolling, settling, trimming,
hidden panels and height changes.

[Issue #486](https://github.com/freenet/river/issues/486) described a bug under
the previous policy, where arrivals were supposed to follow. Its visibility
gate could latch closed after the composer grew or the gap exceeded 100px,
and content changes could miss the scroll trigger. The new policy supersedes
that expectation: preserving the view on arrival is now intentional. The
regression tests retain those triggering events and check that the reader's
place is preserved.

The tradeoff is explicit: even a reader at the end may need to scroll down or
click Latest to reveal a new message. Opening, sending and Latest still take
the reader to the end. Preserving a reading position may change `scrollTop`
when rows are removed or the chat area resizes; the goal is stable visible
content, not an unchanged numeric scroll position.

## Room history

- Opening a room, an own send and Latest request one instant move to the end.
  A pending request carries its room so a room change cannot apply it to the
  next room. It waits for rows and visible layout when necessary.
- After that request lands, an end hold keeps the end visible while rows
  change height, such as images loading or private messages decrypting. An
  arrival ends the hold instead of following. Scrolling upward more than 4px
  from the scroller's absolute end, hiding the panel, changing rooms or an
  empty range also releases it. Accepted for now: padding below the newest
  message lets a small upward scroll release the hold while the message is
  still visible and Latest stays hidden. Later row growth no longer holds
  the view at the end.
- Outside that hold, a change in chat-area height preserves the bottom edge
  of the view. Width-only reflow gets no correction. Paging and panel reveals
  retain their reading-position corrections; deleting the reading row uses
  the row above as the preferred fallback.
- Latest appears when the newest message's bottom is more than 4px below
  the visible area, or a held range withholds newer messages. The margin is
  measured against the message, not the padding below it.
- A room's read marker advances when the tab is visible, the history has
  layout and has completed its reveal restore, the rendered range reaches
  the room's latest message, and that message's bottom is on screen within
  the same 4px margin. Scrolling through a partial range does not advance it.
  Every mark-read path is bounded by the message published in `NEWEST_SEEN`.

Browser scroll anchoring remains disabled on the room scroller. Custom
position corrections remain in place; the two known
[#507](https://github.com/freenet/river/issues/507) cases for late images and
bulk removal above a parked reader remain follow-up work.

## DM threads

DM threads share the explicit navigation policy: opening, own send and
Latest jump instantly; inbound DMs do not trigger scrolling. Latest uses
the same 4px message-bottom margin. The thread updates its read cutoff to
the newest rendered inbound DM timestamp only when the tab is visible and
the newest DM's bottom is on screen.

This shared policy does not imply identical position correction machinery.
DM threads do not currently have the room history's end hold or explicit
composer/keyboard bottom-edge correction. Their timestamp cutoff and its
limitations are described in [direct-messages.md](direct-messages.md).
