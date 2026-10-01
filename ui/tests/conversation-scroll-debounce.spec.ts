import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import {
  DEBOUNCE_SETTLE_MS,
  debounceAdvance,
  debounceDeliver,
  debounceDeliverJoin,
  debounceDistanceFromBottom,
  debounceDrift,
  debounceExpectFallbackSelected,
  debounceFired,
  debounceFrames,
  debounceOnlyPending,
  debounceOpenFilledRoom,
  debouncePending,
  debouncePauseWhenQuiet,
  debounceRestore,
  debounceScrollTo,
  debounceScrollTop,
  debounceState,
  debounceUseFallback,
} from "./history-scroll-fixture";

// The settle fallback for browsers without `scrollend` (Safari before 17.4):
// a gesture settles once `scroll` has been quiet for 120ms
// (SCROLL_SETTLE_DEBOUNCE_MS in ui/src/components/conversation/history_scroll.rs).
// Every engine here has `scrollend`, so each test selects the fallback before
// the app starts and checks that it did; see history-scroll-fixture.ts.
//
// The clock is Playwright's and paused once the room is set up, so every
// deadline below is counted on it from the scroll that last re-armed the
// debounce. Assertions about visible behaviour come first; the timer records
// are supporting evidence, checked last, so a wrong implementation fails at
// what the reader would see.
//
// The scenarios mirror the native ones in conversation-autoscroll.spec.ts
// ("An upward scroll holds new messages until it settles").

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs.
const BOTTOM_THRESHOLD_PX = 100;
/// Slack for fractional layout after a scroll that did land at the bottom.
const AT_BOTTOM_EPSILON_PX = 4;
/// The geometry budget for "the reader's message did not move", as
/// IN_PLACE_TOLERANCE_PX in conversation-autoscroll.spec.ts.
const IN_PLACE_TOLERANCE_PX = 4;
/// An upward move past rounding (SCROLL_TOP_SLACK_PX is 2) and small enough
/// that the move plus one short arrival (`debounceDeliverJoin`) stays inside
/// the follow band.
const UP_PX = 12;
/// Taller than the follow band on its own.
const TALL = (marker: string) =>
  `${marker}\n${Array.from({ length: 12 }, (_, i) => `line ${i}`).join("\n")}`;

/// The scroll registered a settle, which is now the only one pending, and no
/// time has passed since: its deadline is a full 120ms away.
async function expectFreshDeadline(page: Page, why: string) {
  const state = await debounceState(page);
  const pending = debounceOnlyPending(state, why);
  expect(state.now - pending.at, `${why}: the last re-arming scroll is now`).toBe(0);
  return pending;
}

/// The scroll just registered a settle that is still pending, whatever else is.
async function expectNewDeadline(page: Page, why: string) {
  const state = await debounceState(page);
  const latest = state.registrations.at(-1);
  const fresh = !!latest && !latest.cleared && !latest.fired && latest.at === state.now;
  expect(fresh, `${why} (${JSON.stringify(latest)})`).toBe(true);
  return latest!;
}

/// The newest settle still pending: the deadline the next quiet interval runs from.
async function lastRearm(page: Page, why: string) {
  const pending = debouncePending(await debounceState(page));
  expect(pending.length, why).toBeGreaterThan(0);
  return pending.reduce((a, b) => (b.seq > a.seq ? b : a));
}

/// The reader's message is where it was when they scrolled.
async function expectHeld(page: Page, at: { id: string; gap: number }, why: string) {
  expect(await debounceDrift(page, at), why).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
}

/// The reader moves up `px` from where they are; returns what they could see.
async function readerScrollsUp(page: Page, px: number, why: string) {
  const target = (await debounceScrollTop(page)) - px;
  const moved = await debounceScrollTo(page, target);
  expect(moved.scrolled, `premise: ${why}: the scroll event was delivered`).toBe(true);
  expect(Math.abs(moved.landed - target), `premise: ${why}: the scroll landed where aimed`).toBeLessThanOrEqual(1);
  expect(moved.at, `premise: ${why}: a message is visible`).not.toBeNull();
  return moved.at!;
}

async function expectFollowing(page: Page, text: string, why: string) {
  await debounceDeliver(page, text);
  expect(await debounceDistanceFromBottom(page), why).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
}

test.describe("Without scrollend, a gesture settles after 120ms of quiet", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test.beforeEach(async ({ page }) => {
    await debounceUseFallback(page);
  });

  test.afterEach(async ({ page }) => {
    await debounceRestore(page);
  });

  test("an arrival inside the band is held until the quiet deadline, and the next one follows", async ({ page }) => {
    await debounceOpenFilledRoom(page, "Team Chat Room");
    await debounceExpectFallbackSelected(page);
    const setup = await debouncePauseWhenQuiet(page);

    const at = await readerScrollsUp(page, UP_PX, "the upward scroll");
    const armed = await expectFreshDeadline(page, "premise: the upward scroll armed the debounce");
    await debounceDeliverJoin(page);
    await expectHeld(page, at, "an arrival snapped a reader scrolling up inside the band");
    await expectFreshDeadline(page, "premise: the arrival did not re-arm the debounce");

    await debounceAdvance(page, DEBOUNCE_SETTLE_MS - 1);
    expect(debounceFired(await debounceState(page), setup), "the gesture settled before 120ms of quiet").toEqual([]);
    await expectHeld(page, at, "the hold let go before the deadline");

    const beforeSettle = await debounceScrollTop(page);
    await debounceAdvance(page, 1);
    expect(await debounceScrollTop(page), "the settle moved the view").toBe(beforeSettle);
    expect(
      await debounceDistanceFromBottom(page),
      "premise: the reader settled inside the band",
    ).toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX);
    await expectFollowing(page, "arrival after the settle", "a reader who settled inside the band was not followed");

    const fired = debounceFired(await debounceState(page), setup);
    expect(fired.map((r) => r.handle), "the settle that ran is the armed one").toEqual([armed.handle]);
    expect(fired[0].fired!.at - armed.at, "it ran 120ms after the scroll").toBe(DEBOUNCE_SETTLE_MS);
  });

  test("a second scroll inside 120ms moves the deadline; the first one does not settle", async ({ page }) => {
    await debounceOpenFilledRoom(page, "Team Chat Room");
    await debounceExpectFallbackSelected(page);
    const setup = await debouncePauseWhenQuiet(page);

    const half = UP_PX / 2;
    await readerScrollsUp(page, half, "the first upward scroll");
    const first = await expectFreshDeadline(page, "premise: the first scroll armed the debounce");
    await debounceAdvance(page, 60);
    const at = await readerScrollsUp(page, half, "the second upward scroll");
    const second = await expectNewDeadline(page, "premise: the second scroll re-armed the debounce");
    expect(second.at - first.at, "premise: the second scroll re-armed 60ms later").toBe(60);
    expect(
      await debounceDistanceFromBottom(page),
      "premise: the reader is still inside the band",
    ).toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX);

    // Past the first deadline: an arrival is still held, as the gesture is.
    await debounceAdvance(page, first.at + DEBOUNCE_SETTLE_MS + 1 - (await debounceState(page)).now);
    await debounceDeliverJoin(page);
    await expectHeld(page, at, "the first scroll's deadline settled the gesture, so the arrival snapped");

    const pending = debounceOnlyPending(await debounceState(page), "premise: only the second scroll's settle is pending");
    expect(pending.handle, "premise: the pending settle is the second scroll's").toBe(second.handle);
    await debounceAdvance(page, second.at + DEBOUNCE_SETTLE_MS - 1 - (await debounceState(page)).now);
    expect(debounceFired(await debounceState(page), setup), "the gesture settled before the second deadline").toEqual([]);
    await expectHeld(page, at, "the hold let go before the second deadline");
    await debounceAdvance(page, 1);
    expect(
      await debounceDistanceFromBottom(page),
      "premise: the reader settled inside the band",
    ).toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX);
    await expectFollowing(page, "arrival after the second deadline", "the gesture never settled after its quiet interval");

    const state = await debounceState(page);
    const firstRec = state.registrations.find((r) => r.handle === first.handle)!;
    expect(firstRec.fired, "the first deadline fired").toBeNull();
    expect(firstRec.cleared?.duringScroll, "the second scroll cleared the first deadline").toBe(true);
    expect(debounceFired(state, setup).map((r) => r.handle), "only the second deadline settled").toEqual([second.handle]);
  });

  test("a tall arrival while held leaves the settled reader parked through the next arrival", async ({ page }) => {
    await debounceOpenFilledRoom(page, "Team Chat Room");
    await debounceExpectFallbackSelected(page);
    const setup = await debouncePauseWhenQuiet(page);

    const at = await readerScrollsUp(page, UP_PX, "the upward scroll");
    const armed = await expectFreshDeadline(page, "premise: the upward scroll armed the debounce");
    await debounceDeliver(page, TALL("tall arrival while held"));
    await expectHeld(page, at, "the tall arrival snapped a reader scrolling up");
    expect(
      await debounceDistanceFromBottom(page),
      "premise: the tall arrival leaves the reader outside the band",
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    await expectFreshDeadline(page, "premise: the arrival did not re-arm the debounce");

    await debounceAdvance(page, DEBOUNCE_SETTLE_MS - 1);
    expect(debounceFired(await debounceState(page), setup), "the gesture settled before 120ms of quiet").toEqual([]);
    const beforeSettle = await debounceScrollTop(page);
    await debounceAdvance(page, 1);
    expect(debounceFired(await debounceState(page), setup).map((r) => r.handle), "premise: the gesture settled").toEqual([
      armed.handle,
    ]);
    expect(await debounceScrollTop(page), "the settle moved the view").toBe(beforeSettle);
    await expectHeld(page, at, "the settle moved the reader's message");

    await debounceDeliver(page, `arrival after the settle: ${"r".repeat(200)}`);
    await expectHeld(page, at, "the settle re-measured the pin outside the band, so the arrival snapped");
  });

  // The new room's opening snap is itself a scroll, and the debounce clears
  // whatever handle it still holds before re-arming, so an old settle the switch
  // merely FORGOT to clear is cleared there anyway: the visible check bites when
  // the handle is lost uncleared, and the timer record shows the switch cleared
  // it itself, not the snap's scroll.
  test("a room switch cancels the pending settle, which cannot end a gesture in the new room", async ({ page }) => {
    await debounceOpenFilledRoom(page, "Team Chat Room", "/?deep-history-room=1");
    await debounceExpectFallbackSelected(page);
    const setup = await debouncePauseWhenQuiet(page);

    await readerScrollsUp(page, UP_PX, "the upward scroll in the old room");
    const old = await expectFreshDeadline(page, "premise: the old room's scroll armed the debounce");

    await debounceAdvance(page, 40);
    const atSwitch = debounceOnlyPending(await debounceState(page), "premise: a settle is pending at the switch");
    expect(atSwitch.handle, "premise: the pending settle is the old room's").toBe(old.handle);
    await callRiverTest(page, "switchRoom", "Deep History Room");
    await page.clock.runFor(0);
    await expect(page.getByRole("heading", { name: "Deep History Room" })).toBeVisible();
    await debounceFrames(page, 3);
    // Let the new room's opening work run (deferred writes), well short of the
    // old deadline.
    await debounceAdvance(page, 5);
    expect(
      await debounceDistanceFromBottom(page),
      "premise: the new room opened at its newest message",
    ).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
    // The new room's newest message can be from yesterday in the browser's
    // timezone, and then the next arrival also brings a "Today" divider, pushing
    // the held join below out of the band. Let one arrival put it in place.
    await expectFollowing(page, "first arrival in the new room", "premise: the new room follows before the gesture");

    // A fresh gesture in the new room, whose own deadline is after the old one.
    const at = await readerScrollsUp(page, UP_PX, "the upward scroll in the new room");
    // Not "the only one pending": whether the old one still is, is the question.
    const fresh = await expectNewDeadline(page, "premise: the new room's scroll armed the debounce");
    expect(fresh.at + DEBOUNCE_SETTLE_MS, "premise: the fresh deadline is after the old one").toBeGreaterThan(
      old.at + DEBOUNCE_SETTLE_MS + 1,
    );

    // Past the old deadline: the fresh gesture still holds an arrival.
    await debounceAdvance(page, old.at + DEBOUNCE_SETTLE_MS + 1 - (await debounceState(page)).now);
    await debounceDeliverJoin(page);
    await expectHeld(page, at, "the old room's deadline settled the new room's gesture, so the arrival snapped");

    // The fresh gesture settles 120ms after the last scroll that re-armed it. In
    // this windowed room the held arrival moves rows above the reader, and the
    // anchor correction is itself a scroll, so that can be later than `fresh`.
    const settleFrom = await lastRearm(page, "premise: the fresh gesture has a settle pending");
    expect(settleFrom.seq, "premise: the pending settle is the fresh gesture's or later").toBeGreaterThanOrEqual(fresh.seq);
    await debounceAdvance(page, settleFrom.at + DEBOUNCE_SETTLE_MS - 1 - (await debounceState(page)).now);
    expect(debounceFired(await debounceState(page), setup), "a settle ran before the fresh deadline").toEqual([]);
    await expectHeld(page, at, "the fresh hold let go before its deadline");
    await debounceAdvance(page, 1);
    expect(
      await debounceDistanceFromBottom(page),
      "premise: the reader settled inside the band",
    ).toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX);
    await expectFollowing(page, "arrival after the fresh settle", "the new room's gesture never settled");

    const state = await debounceState(page);
    const oldRec = state.registrations.find((r) => r.handle === old.handle)!;
    expect(oldRec.fired, "the old room's deadline fired").toBeNull();
    expect(oldRec.cleared, "the old room's deadline was never cleared").not.toBeNull();
    expect(
      oldRec.cleared!.duringScroll,
      "the old room's deadline was cleared by a later scroll, not by the room switch",
    ).toBe(false);
    expect(debounceFired(state, setup).map((r) => r.handle), "only the fresh gesture's last deadline settled").toEqual([
      settleFrom.handle,
    ]);
  });
});
