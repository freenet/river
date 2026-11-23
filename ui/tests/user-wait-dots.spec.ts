import { test, expect, Locator, Page } from "@playwright/test";
import {
  clickFooter,
  committedRow,
  memberRows,
  narrowLayout,
  openOwnMessageEdit,
  openRoomWithComposer,
  openRoomWithTarget,
  openSheetFromRow,
  reactTo,
  revealFooter,
  selectRoom,
  settle,
  sheet,
  submitMessage,
  waitForApp,
} from "./example-room";
import {
  AT_BOTTOM_EPSILON_PX,
  distanceFromBottom,
  expectParkedAwayFromEnd,
  expectReadingRow,
  expectSettledAtBottom,
  fillHistory,
  readerScrollsWithoutGesture,
  rowOffset,
  READING_ROW_BUDGET_PX,
  scrollMetrics,
  WELL_AWAY_FROM_END_PX,
} from "./history-geometry";
import { callRiverTest } from "./river-test";

// The ten wave dots that say an edit, a delete, a reaction or a join is still
// on its way (`user_wait_dots.rs`, `app/user_waits.rs`). One host paints at a
// time: the open room's dock above the composer, else the Welcome slot, and
// nothing while something covers the room. Dots appear 300 ms after activity,
// fade over 200 ms, and vanish at once when their host stops being the owner.
// The room spacer after `bottom-sentinel` makes room for the dock.
//
// The no-sync build starts Disconnected; the dots show only while connected,
// so every test connects first unless it is testing the disconnected case.

/// `user-wait-spacer`'s open height: the dot row and its gap (main.css).
const SPACER_OPEN_PX = 26;
/// main.css `--user-wait-delay` and `--user-wait-fade`.
const DELAY_MS = 300;
const FADE_MS = 200;

const roomDots = (page: Page) => page.locator('[data-testid="user-wait-dots"][data-surface="room"]');
const welcomeDots = (page: Page) => page.locator('[data-testid="user-wait-dots"][data-surface="welcome"]');
const spacer = (page: Page) => page.getByTestId("user-wait-spacer");
const status = (page: Page) => page.locator("#user-wait-status");
const marks = (page: Page) => callRiverTest(page, "userWaitRooms");

async function connect(page: Page) {
  await callRiverTest(page, "setSyncStatus", "connected");
}

/// The host is the painting one: the owner, with activity.
async function expectShowing(host: Locator, why: string) {
  await expect(host, why).toHaveAttribute("data-active", "true");
  await expect(host, why).toHaveAttribute("data-owner", "true");
}

async function expectIdle(page: Page, why: string) {
  await expect(roomDots(page), why).toHaveAttribute("data-active", "false");
  await expect(spacer(page), why).toHaveAttribute("data-active", "false");
}

/// The marks drop to `n` (they are written in a deferred closure).
async function expectMarks(page: Page, n: number, why: string) {
  await expect.poll(() => marks(page), { message: why, timeout: 5_000 }).toBe(n);
}

/// Run every running transition on `el` (and the spacer) to its end, so its
/// computed style is where it rests.
function finishTransitions(page: Page) {
  return page.evaluate(() => {
    for (const el of document.querySelectorAll('[data-testid="user-wait-dots"], #user-wait-spacer')) {
      for (const a of el.getAnimations()) a.finish();
    }
  });
}

type TransitionInfo = { property: string; delay: number; duration: number; from: string | null; to: string | null };

/// `el`'s CSS transitions now, and its computed opacity, read in one task.
function paintState(host: Locator) {
  return host.evaluate((el) => ({
    opacity: getComputedStyle(el).opacity,
    visibility: getComputedStyle(el).visibility,
    transitions: el.getAnimations().map((a) => {
      const t = a as CSSTransition;
      const timing = t.effect!.getComputedTiming();
      const frames = (t.effect as KeyframeEffect).getKeyframes();
      const prop = t.transitionProperty;
      return {
        property: prop,
        delay: Number(timing.delay),
        duration: Number(timing.duration),
        from: (frames[0]?.[prop] as string | undefined) ?? null,
        to: (frames.at(-1)?.[prop] as string | undefined) ?? null,
      } as TransitionInfo;
    }),
  }));
}

type PaintState = { opacity: string; visibility: string; transitions: TransitionInfo[] };

/// From now on, record a host's paint state in the same task as the first DOM
/// change that brings it to `moment` ("shown": owner with activity, including
/// a host that mounts that way; "unowned": active but no longer the owner).
/// Before any frame has painted, so a full-opacity first paint or a fade on
/// losing ownership shows, however slow the surface around it is. Read with
/// `takeRecordedPaint`.
function recordPaintAt(page: Page, surface: "room" | "welcome", moment: "shown" | "unowned") {
  return page.evaluate(
    ({ surface, moment }) => {
      const w = window as unknown as { __waitProbe?: unknown; __waitObserver?: MutationObserver };
      w.__waitProbe = null;
      w.__waitObserver?.disconnect();
      const selector = `[data-testid="user-wait-dots"][data-surface="${surface}"]`;
      const take = () => {
        const el = document.querySelector(selector);
        if (!el || el.getAttribute("data-active") !== "true") return;
        if (el.getAttribute("data-owner") !== (moment === "shown" ? "true" : "false")) return;
        w.__waitObserver!.disconnect();
        // getAnimations flushes style: the transitions this change starts.
        const transitions = el.getAnimations().map((a) => {
          const t = a as CSSTransition;
          const frames = (t.effect as KeyframeEffect).getKeyframes();
          const prop = t.transitionProperty;
          return {
            property: prop,
            delay: Number(t.effect!.getComputedTiming().delay),
            duration: Number(t.effect!.getComputedTiming().duration),
            from: (frames[0]?.[prop] as string | undefined) ?? null,
            to: (frames.at(-1)?.[prop] as string | undefined) ?? null,
          };
        });
        const cs = getComputedStyle(el);
        w.__waitProbe = { opacity: cs.opacity, visibility: cs.visibility, transitions };
      };
      w.__waitObserver = new MutationObserver(take);
      w.__waitObserver.observe(document.body, {
        subtree: true,
        childList: true,
        attributes: true,
        attributeFilter: ["data-active", "data-owner"],
      });
    },
    { surface, moment },
  );
}

async function takeRecordedPaint(page: Page): Promise<PaintState> {
  let probe: PaintState | null = null;
  await expect
    .poll(async () => (probe = await page.evaluate(() => (window as unknown as { __waitProbe: never }).__waitProbe)) !== null, {
      message: "the host reached the recorded moment",
      timeout: 5_000,
    })
    .toBe(true);
  return probe!;
}

function opacityTransition(transitions: TransitionInfo[]): TransitionInfo | undefined {
  return transitions.find((t) => t.property === "opacity");
}

/// The delayed appearance: an opacity transition from 0 to 1 after 300 ms.
function expectDelayedAppearance(state: PaintState, why: string) {
  const t = opacityTransition(state.transitions);
  expect(t, `${why}: an opacity transition`).toBeDefined();
  expect(t!.delay, `${why}: it waits 300 ms`).toBe(DELAY_MS);
  expect(t!.duration, `${why}: and fades in over 200 ms`).toBe(FADE_MS);
  expect(t!.to, `${why}: to full opacity`).toBe("1");
  expect(Number(state.opacity), `${why}: not painted at full opacity first`).toBeLessThan(1);
}

/// A room with a received message at the end, connected, and no mark yet.
async function connectedRoomWithTarget(page: Page): Promise<Locator> {
  const row = await openRoomWithTarget(page);
  await connect(page);
  await expectIdle(page, "premise: nothing on its way yet");
  expect(await marks(page), "premise: no marks").toBe(0);
  return row;
}

/// React to `row` and wait for the room's mark.
async function reactAndMark(page: Page, row: Locator) {
  await reactTo(page, row);
  await expectMarks(page, 1, "the reaction marks its room");
}

/// An own committed message in the current room, sent from the composer.
async function sendOwn(page: Page, text: string): Promise<Locator> {
  const row = await submitMessage(page, text);
  await expect(row).toHaveCount(1, { timeout: 5_000 });
  await expect(page.getByTestId("message-input")).toHaveValue("");
  return row;
}

/// Delete `row` through its footer Delete and the confirmation's Delete.
async function confirmDelete(page: Page, row: Locator) {
  await clickFooter(page, row, "message-delete-button");
  const dialog = deleteDialog(page);
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "Delete", exact: true }).click();
  await expect(dialog).toHaveCount(0);
}

function deleteDialog(page: Page): Locator {
  return page.locator("div.fixed.inset-0").filter({ has: page.getByRole("heading", { name: "Delete Message?" }) });
}

/// No two hosts paint at once.
async function expectAtMostOnePainter(page: Page) {
  const painters = await page.locator('[data-testid="user-wait-dots"][data-owner="true"][data-active="true"]').count();
  expect(painters, "at most one host paints").toBeLessThanOrEqual(1);
}

test.describe("Room changes on their way", { tag: "@chromium-only" }, () => {
  test("a reaction shows the room's dots until the room's UPDATE reply", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await reactAndMark(page, row);
    await expectShowing(roomDots(page), "a reaction shows the dots");
    await expect(roomDots(page)).toHaveAttribute("data-reason", "sending");
    await expect(spacer(page)).toHaveAttribute("data-active", "true");
    await expect(status(page), "room changes are not announced").toHaveText("");

    await callRiverTest(page, "deliverUpdateResponse");
    await expectMarks(page, 0, "the reply ends the mark");
    await expectIdle(page, "the reply ends the dots");
    await expect(roomDots(page)).toHaveAttribute("data-reason", "");
  });

  test("an edit marks its room", async ({ page }) => {
    await openRoomWithComposer(page);
    await connect(page);
    const row = await sendOwn(page, "edit me");
    expect(await marks(page), "premise: the send marked nothing").toBe(0);
    const editArea = await openOwnMessageEdit(page, row);
    await editArea.fill("edited once");
    await page.getByTestId("edit-save-button").click();
    await expect(committedRow(page, "edited once")).toHaveCount(1);
    await expectMarks(page, 1, "the edit marks its room");
    await expectShowing(roomDots(page), "an edit shows the dots");
    await callRiverTest(page, "deliverUpdateResponse");
    await expectIdle(page, "the reply ends them");
  });

  test("a delete through its confirmation marks its room", async ({ page }) => {
    await openRoomWithComposer(page);
    await connect(page);
    const row = await sendOwn(page, "delete me");
    await confirmDelete(page, row);
    await expect(committedRow(page, "delete me")).toHaveCount(0);
    await expectMarks(page, 1, "the delete marks its room");
    await expectShowing(roomDots(page), "a delete shows the dots once the confirmation closes");
    await callRiverTest(page, "deliverUpdateResponse");
    await expectIdle(page, "the reply ends them");
  });

  test("while disconnected the mark is kept but nothing shows; reconnecting shows it again", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    // The no-sync build starts Disconnected.
    await reactAndMark(page, row);
    await settle(page);
    await expectIdle(page, "nothing shows while disconnected");

    await connect(page);
    await expectShowing(roomDots(page), "connected, the mark shows");
    await expect(spacer(page)).toHaveAttribute("data-active", "true");

    for (const s of ["connecting", "disconnected", "error"] as const) {
      await callRiverTest(page, "setSyncStatus", s);
      await expect(roomDots(page), `${s}: hidden`).toHaveAttribute("data-active", "false");
      expect(await marks(page), `${s}: the mark is kept`).toBe(1);
    }
    await connect(page);
    await expectShowing(roomDots(page), "a reconnect shows the pending mark again");
  });

  for (const { end, ends } of [
    { end: "a keyed UPDATE error", ends: true },
    { end: "an unkeyed UPDATE failure", ends: true },
    { end: "a GET error that mentions an update", ends: false },
    { end: "the 20 s cap", ends: true },
  ]) {
    test(`${end} ${ends ? "ends" : "does not end"} the mark`, async ({ page }) => {
      const row = await connectedRoomWithTarget(page);
      await reactAndMark(page, row);
      await expectShowing(roomDots(page), "premise: showing");
      if (end.startsWith("a keyed")) await callRiverTest(page, "deliverUpdateError", "keyed");
      else if (end.startsWith("an unkeyed")) await callRiverTest(page, "deliverUpdateError", "unkeyed");
      else if (end.startsWith("a GET")) await callRiverTest(page, "deliverUpdateError", "get");
      else await callRiverTest(page, "expireUserWaits");
      if (ends) {
        await expectMarks(page, 0, `${end} ends the mark`);
        await expectIdle(page, `${end} ends the dots`);
      } else {
        await settle(page);
        expect(await marks(page), `${end} leaves the mark`).toBe(1);
        await expectShowing(roomDots(page), `${end} leaves the dots`);
      }
    });
  }

  test("an unkeyed UPDATE failure ends every room's mark", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await reactAndMark(page, row);
    await selectRoom(page, "Team Chat Room");
    const team = await sendOwn(page, "team target");
    await reactTo(page, team);
    await expectMarks(page, 2, "premise: two rooms marked");
    await callRiverTest(page, "deliverUpdateError", "unkeyed");
    await expectMarks(page, 0, "an unkeyed failure names no room, so it ends them all");
  });

  test("a keyed reply ends only its own room's mark", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await reactAndMark(page, row);
    await selectRoom(page, "Team Chat Room");
    await expectIdle(page, "another room shows nothing");
    // The reply is for Team Chat Room's contract, which has no mark.
    await callRiverTest(page, "deliverUpdateResponse");
    await settle(page);
    expect(await marks(page), "Your Private Room keeps its mark").toBe(1);
  });

  test("sending messages never lights the dots", async ({ page }) => {
    await openRoomWithComposer(page);
    await connect(page);
    await submitMessage(page, "plain send one");
    await submitMessage(page, "plain send two");
    await expect(committedRow(page, "plain send one")).toHaveCount(1);
    await expect(committedRow(page, "plain send two")).toHaveCount(1);
    await settle(page);
    expect(await marks(page), "a send marks nothing").toBe(0);
    await expectIdle(page, "a send shows nothing");
  });

  test("a room's mark shows only in that room", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await reactAndMark(page, row);
    await expectShowing(roomDots(page), "premise: showing in its room");

    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await expect(page.getByRole("heading", { name: "Team Chat Room" })).toBeVisible();
    await expectIdle(page, "another room shows nothing");
    expect(await marks(page)).toBe(1);

    await callRiverTest(page, "switchRoom", "Your Private Room");
    await expect(page.getByRole("heading", { name: "Your Private Room" })).toBeVisible();
    await expectShowing(roomDots(page), "back in its room, it shows again");
  });
});

test.describe("Joining a room", () => {
  test("the Welcome screen shows and announces a join while connected", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await expect(page.getByRole("heading", { name: "Welcome to River" })).toBeVisible({ timeout: 10_000 });
    await expect(welcomeDots(page)).toHaveAttribute("data-owner", "true");
    await expect(welcomeDots(page)).toHaveAttribute("data-active", "false");

    await callRiverTest(page, "setPendingJoin", true);
    await settle(page);
    await expect(welcomeDots(page), "nothing while disconnected").toHaveAttribute("data-active", "false");
    await expect(status(page)).toHaveText("");

    await connect(page);
    await expectShowing(welcomeDots(page), "a join shows on the Welcome screen");
    await expect(welcomeDots(page)).toHaveAttribute("data-reason", "joining-room");
    await expect(status(page), "a join is announced").toHaveText("Joining room");
    await expect(status(page)).toHaveAttribute("role", "status");
    await expect(welcomeDots(page)).toHaveAttribute("aria-hidden", "true");

    await callRiverTest(page, "setPendingJoin", false);
    await expect(welcomeDots(page)).toHaveAttribute("data-active", "false");
    await expect(welcomeDots(page)).toHaveAttribute("data-reason", "");
    await expect(status(page)).toHaveText("");
  });

  test("a join outranks the room's own change", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await reactAndMark(page, row);
    await callRiverTest(page, "setPendingJoin", true);
    await expect(roomDots(page)).toHaveAttribute("data-reason", "joining-room");
    await expect(status(page)).toHaveText("Joining room");
    await callRiverTest(page, "setPendingJoin", false);
    await expect(roomDots(page)).toHaveAttribute("data-reason", "sending");
    await expect(status(page)).toHaveText("");
  });

  test("the Welcome slot's dots appear after the delay, from nothing", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await expect(page.getByRole("heading", { name: "Welcome to River" })).toBeVisible({ timeout: 10_000 });
    await connect(page);
    await settle(page);
    await recordPaintAt(page, "welcome", "shown");
    await callRiverTest(page, "setPendingJoin", true);
    expectDelayedAppearance(await takeRecordedPaint(page), "a join on the Welcome screen");

    await finishTransitions(page);
    expect((await paintState(welcomeDots(page))).opacity, "once shown").toBe("1");
    await callRiverTest(page, "setPendingJoin", false);
    await expect(welcomeDots(page)).toHaveAttribute("data-active", "false");
    await finishTransitions(page);
    expect((await paintState(welcomeDots(page))).opacity, "once faded").toBe("0");
  });

  test("a room dock mounting during a join waits before it paints", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await expect(page.getByRole("heading", { name: "Welcome to River" })).toBeVisible({ timeout: 10_000 });
    await connect(page);
    await callRiverTest(page, "setPendingJoin", true);
    await expectShowing(welcomeDots(page), "premise: the join shows on the Welcome screen");
    await expect(roomDots(page), "premise: no room dock yet").toHaveCount(0);

    await recordPaintAt(page, "room", "shown");
    await selectRoom(page, "Your Private Room");
    // The dock mounts already active: @starting-style makes it fade in after
    // the delay too, instead of painting at once.
    expectDelayedAppearance(await takeRecordedPaint(page), "a dock mounting during a join");
    await expect(welcomeDots(page)).toHaveCount(0);
    await callRiverTest(page, "setPendingJoin", false);
  });
});

test.describe("Paint timing", () => {
  test("dots fade in after the delay, fade out when done, and a new change during the fade does not jump", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await recordPaintAt(page, "room", "shown");
    await reactAndMark(page, row);
    expectDelayedAppearance(await takeRecordedPaint(page), "a reaction");
    await finishTransitions(page);
    expect((await paintState(roomDots(page))).opacity, "shown once the delay and fade-in end").toBe("1");
    await expect(spacer(page)).toHaveCSS("height", `${SPACER_OPEN_PX}px`);

    // The reply: a 200 ms fade, no delay. Held half way.
    await callRiverTest(page, "deliverUpdateResponse");
    await expect(roomDots(page)).toHaveAttribute("data-active", "false");
    const fading = await roomDots(page).evaluate((el) => {
      const t = el.getAnimations().find((a) => (a as CSSTransition).transitionProperty === "opacity") as CSSTransition | undefined;
      if (!t) return null;
      const timing = t.effect!.getComputedTiming();
      t.pause();
      t.currentTime = Number(timing.delay) + Number(timing.duration) / 2;
      return { delay: Number(timing.delay), duration: Number(timing.duration), opacity: Number(getComputedStyle(el).opacity) };
    });
    expect(fading, "the end fades").not.toBeNull();
    expect(fading!.delay, "at once").toBe(0);
    expect(fading!.duration).toBe(FADE_MS);
    expect(fading!.opacity, "premise: half way through the fade").toBeGreaterThan(0.05);
    expect(fading!.opacity).toBeLessThan(0.95);

    // Busy again mid-fade (the chip's own click takes the reaction back,
    // another eligible change): no jump back to full opacity.
    await recordPaintAt(page, "room", "shown");
    await row.getByTestId("reaction-chip").first().click();
    const again = await takeRecordedPaint(page);
    expect(Number(again.opacity), "busy again does not jump to full opacity").toBeLessThan(0.95);
    const t = opacityTransition(again.transitions);
    expect(t, "it transitions back up").toBeDefined();
    expect(t!.to).toBe("1");
    expect(t!.delay, "after the delay").toBe(DELAY_MS);

    await finishTransitions(page);
    expect((await paintState(roomDots(page))).opacity).toBe("1");
    await callRiverTest(page, "deliverUpdateResponse");
    await expect(roomDots(page)).toHaveAttribute("data-active", "false");
    await finishTransitions(page);
    const done = await paintState(roomDots(page));
    expect(done.opacity, "faded out").toBe("0");
    expect(done.visibility).toBe("hidden");
    await expect(spacer(page)).toHaveCSS("height", "0px");
  });
});

/// Wait until the room host paints at full opacity, finished.
async function expectPainted(page: Page) {
  await expectShowing(roomDots(page), "premise: showing");
  await finishTransitions(page);
  expect((await paintState(roomDots(page))).opacity, "premise: painted").toBe("1");
}

/// `cover` takes ownership from the room host: at that instant it stops
/// painting, with no fade, while its activity goes on, and the spacer starts
/// closing.
async function expectSuppressedAtOnce(page: Page, why: string, cover: () => Promise<void>) {
  await recordPaintAt(page, "room", "unowned");
  await cover();
  const s = await takeRecordedPaint(page);
  expect(s.opacity, `${why}: no paint at once`).toBe("0");
  expect(s.visibility, `${why}: hidden at once`).toBe("hidden");
  expect(opacityTransition(s.transitions), `${why}: no fade`).toBeUndefined();
  await expect(roomDots(page), why).toHaveAttribute("data-owner", "false");
  await expect(roomDots(page), `${why}: still active, logically`).toHaveAttribute("data-active", "true");
  await expect(spacer(page), why).toHaveAttribute("data-active", "false");
  await expectAtMostOnePainter(page);
}

async function expectOwnerAgain(page: Page, why: string) {
  await expectShowing(roomDots(page), why);
  await expect(spacer(page), why).toHaveAttribute("data-active", "true");
  await expectAtMostOnePainter(page);
}

test.describe("Ownership", () => {
  test("the delete confirmation hides the room's dots at once", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await reactAndMark(page, row);
    await expectPainted(page);
    const own = page.locator('[id^="msg-"][data-message-self="true"]').last();
    await own.scrollIntoViewIfNeeded();
    await expectSuppressedAtOnce(page, "the delete confirmation covers the room", async () => {
      await clickFooter(page, own, "message-delete-button");
      await expect(deleteDialog(page)).toBeVisible();
    });

    await deleteDialog(page).getByRole("button", { name: "Cancel" }).click();
    await expect(deleteDialog(page)).toHaveCount(0);
    await expectOwnerAgain(page, "closing it gives the dots back");
  });

  test("the member info modal hides the room's dots at once", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    test.skip(await narrowLayout(page), "below 48rem the member list replaces the chat (the panel test covers it)");
    await reactAndMark(page, row);
    await expectPainted(page);
    await expectSuppressedAtOnce(page, "the member info modal covers the room", async () => {
      await memberRows(page).first().click();
      await expect(page.getByTestId("member-info-modal")).toBeVisible();
    });
    await page.getByTestId("member-info-close-button").click();
    await expect(page.getByTestId("member-info-modal")).toHaveCount(0);
    await expectOwnerAgain(page, "closing it gives the dots back");
  });

  test("a DM thread keeps the room's dots hidden while it is open", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await connect(page);
    const row = await sendOwn(page, "dm cover target");
    await reactAndMark(page, row);
    await expectPainted(page);

    const other = memberRows(page).filter({ hasNot: page.getByTestId("member-list-self") }).first();
    await expectSuppressedAtOnce(page, "member info, on the way to the thread, covers the room", async () => {
      await other.click();
      await expect(page.getByTestId("member-info-modal")).toBeVisible();
    });
    await page.locator('button[aria-label="Send direct message"]').first().click();
    await expect(page.getByTestId("dm-thread-modal")).toBeVisible();
    await expect(page.getByTestId("member-info-modal")).toHaveCount(0);
    await settle(page);
    await expect(roomDots(page), "the thread covers the room").toHaveAttribute("data-owner", "false");
    expect((await paintState(roomDots(page))).opacity).toBe("0");
    await expect(spacer(page)).toHaveAttribute("data-active", "false");
    await expectAtMostOnePainter(page);

    await page.getByTestId("dm-thread-close-button").click();
    await expect(page.getByTestId("dm-thread-modal")).toHaveCount(0);
    await expectOwnerAgain(page, "closing the thread gives the dots back");
  });

  test("below 48rem the message sheet and the members panel hide the room's dots at once", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    const row = await connectedRoomWithTarget(page);
    expect(await narrowLayout(page), "premise: below 48rem").toBe(true);
    await reactAndMark(page, row);
    await expect(sheet(page)).toBeHidden();
    await expectPainted(page);

    await expectSuppressedAtOnce(page, "the message sheet covers the room", () => openSheetFromRow(page, row));
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await expectOwnerAgain(page, "the sheet closed");

    await expectPainted(page);
    await expectSuppressedAtOnce(page, "the members panel replaces the chat", () =>
      callRiverTest(page, "setMobileView", "members"),
    );
    await callRiverTest(page, "setMobileView", "chat");
    await expectOwnerAgain(page, "back to the chat");
  });
});

/// The newest row in the history and the dots, in viewport px.
function endGeometry(page: Page) {
  return page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    const rows = c.querySelectorAll<HTMLElement>("[data-item-key]");
    const newest = rows[rows.length - 1].getBoundingClientRect();
    const dots = document.querySelector('[data-testid="user-wait-dots"][data-surface="room"]')!.getBoundingClientRect();
    const view = c.getBoundingClientRect();
    return {
      newestBottom: newest.bottom,
      viewBottom: view.bottom,
      dotsTop: dots.top,
      dotsBottom: dots.bottom,
      overflowX: c.scrollWidth - c.clientWidth,
    };
  });
}

test.describe("History", () => {
  test("held at the end, the newest message stays in view above the dots", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await expectSettledAtBottom(page, "premise: at the end");
    await reactAndMark(page, row);
    await expectShowing(roomDots(page), "premise: showing");
    await finishTransitions(page);
    await expect(spacer(page)).toHaveCSS("height", `${SPACER_OPEN_PX}px`);
    await settle(page);

    await expectSettledAtBottom(page, "the end hold takes in the spacer");
    const g = await endGeometry(page);
    expect(g.newestBottom, "the newest message is in view").toBeLessThanOrEqual(g.viewBottom + 1);
    expect(g.dotsTop, "the dots sit below the newest message").toBeGreaterThanOrEqual(g.newestBottom - 0.5);
    expect(g.dotsBottom, "and inside the history's view").toBeLessThanOrEqual(g.viewBottom + 0.5);
    expect(g.overflowX, "no horizontal overflow").toBeLessThanOrEqual(0);
    await expect(page.getByTestId("scroll-to-bottom"), "Latest stays hidden").toHaveCount(0);

    await callRiverTest(page, "deliverUpdateResponse");
    await expect(spacer(page)).toHaveAttribute("data-active", "false");
    await finishTransitions(page);
    await expect(spacer(page), "the spacer collapses after the reply").toHaveCSS("height", "0px");
    await settle(page);
    await expectSettledAtBottom(page, "still at the end once it collapses");
    await expect(page.getByTestId("scroll-to-bottom")).toHaveCount(0);
  });

  test("the spacer follows the sentinel, keyless, and never moves a parked reader", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    const order = await page.evaluate(() => {
      const s = document.getElementById("bottom-sentinel")!;
      const next = s.nextElementSibling;
      return {
        parent: s.parentElement!.id,
        next: next?.id ?? null,
        keys: next ? [next.getAttribute("data-item-key"), next.getAttribute("data-anchor-key")] : null,
      };
    });
    expect(order, "the spacer comes right after bottom-sentinel, with no row or anchor key").toEqual({
      parent: "chat-content",
      next: "user-wait-spacer",
      keys: [null, null],
    });

    await reactAndMark(page, row);
    // Disconnected, the mark stays and the spacer closes: the reader parks
    // with it closed, then it opens and closes under them.
    await callRiverTest(page, "setSyncStatus", "disconnected");
    await expect(spacer(page)).toHaveAttribute("data-active", "false");
    await finishTransitions(page);
    await expect(spacer(page)).toHaveCSS("height", "0px");
    await fillHistory(page);
    const { top } = await scrollMetrics(page);
    await readerScrollsWithoutGesture(page, top - 400);
    await expectParkedAwayFromEnd(page);
    const reading = await expectReadingRow(page);

    await connect(page);
    await expect(spacer(page)).toHaveAttribute("data-active", "true");
    await finishTransitions(page);
    await expect(spacer(page)).toHaveCSS("height", `${SPACER_OPEN_PX}px`);
    await settle(page);
    const opened = await rowOffset(page, reading.key);
    expect(Math.abs(opened! - reading.top), "opening does not move the reader").toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
    expect(await distanceFromBottom(page)).toBeGreaterThan(WELL_AWAY_FROM_END_PX);

    await callRiverTest(page, "deliverUpdateResponse");
    await expect(spacer(page)).toHaveAttribute("data-active", "false");
    await finishTransitions(page);
    await expect(spacer(page)).toHaveCSS("height", "0px");
    await settle(page);
    const closed = await rowOffset(page, reading.key);
    expect(Math.abs(closed! - reading.top), "closing does not move the reader").toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
    expect((await scrollMetrics(page)).top, "premise: still parked").toBeLessThan((await scrollMetrics(page)).max - AT_BOTTOM_EPSILON_PX);
  });
});

test.describe("Hits", () => {
  test("the dots take no clicks", async ({ page }) => {
    const row = await connectedRoomWithTarget(page);
    await reactAndMark(page, row);
    await expectPainted(page);

    const hit = await roomDots(page).evaluate((el) => {
      const r = el.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {
        onDots: !!at && (el.contains(at) || !!at.closest(".user-wait-dock")),
        pointerEvents: [getComputedStyle(el).pointerEvents, getComputedStyle(el.closest(".user-wait-dock")!).pointerEvents],
      };
    });
    expect(hit.onDots, "a hit at the dots' centre goes through them").toBe(false);
    expect(hit.pointerEvents).toEqual(["none", "none"]);

    await page.getByTestId("message-input").fill("through the dots");
    await page.getByTestId("send-message-button").click({ trial: true });
    if (!(await narrowLayout(page))) {
      await revealFooter(page, row);
      await row.getByTestId("message-reply-button").click({ trial: true });
    }
    await row.getByTestId("message-react-button").click({ trial: true });
  });
});
