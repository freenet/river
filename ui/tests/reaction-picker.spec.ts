import { test, expect, Locator, Page } from "@playwright/test";
import { asTouchTablet, coarsePointer, committedRow, DRAG_PX, Fingers, nextFrames, openRoomWithComposer, revealFooter, rowKey, settle, submitMessage, wheelCancelled, withFingers } from "./example-room";
import { deliver, deliverOffscreen, expectFollowed, readerScrollsWithoutGesture, scrollMetrics } from "./history-geometry";
import {
  expectAtReact,
  expectEveryEmojiUsable,
  expectInvokerUncovered,
  measurePicker,
  placement,
  PICKER_GAP,
} from "./reaction-picker-geometry";
import { callRiverTest } from "./river-test";

// The conversation's one reaction picker: a native `popover="auto"`, opened
// at a message's footer React and anchored to it (reaction_picker.rs,
// `.reaction-picker` in main.css). The browser closes it on Escape and on a
// press outside it, which still reaches what was pressed; a pick, a room
// switch and its message leaving the history close it too. While it is open
// the reader's wheel and one-finger touch scrolling are held off, over the
// picker too, as the full-screen backdrop it replaced did, but a zoom (a Ctrl
// wheel, a pinch) is not, arrivals are still followed, and closing lets go of
// all of it.
//
// The picker exists from 48rem: below that a React opens the mobile reaction
// popup instead (mobile-reaction-popup.spec.ts). So a phone project runs
// these at a touch tablet's width, keeping its touch input: the wide
// coarse-pointer case.
test.beforeEach(async ({ page }) => {
  const vp = page.viewportSize();
  if (vp && vp.width === test.info().project.use.viewport?.width) await asTouchTablet(page);
});

const PICKER = '[data-testid="emoji-picker"]';

function picker(page: Page): Locator {
  return page.getByTestId("emoji-picker");
}

function isOpen(page: Page): Promise<boolean> {
  return page.evaluate((sel) => {
    const p = document.querySelector(sel);
    return !!p && p.matches(":popover-open");
  }, PICKER);
}

/// The row whose React has `aria-expanded="true"`, by id; none when closed.
function expandedRows(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('[data-testid="message-react-button"][aria-expanded="true"]')).map(
      (b) => b.closest('[id^="msg-"]')!.id,
    ),
  );
}

async function expectOpenFor(page: Page, row: Locator) {
  await expect.poll(() => isOpen(page), { message: "the picker is open" }).toBe(true);
  await expect.poll(() => expandedRows(page)).toEqual([await row.getAttribute("id")]);
}

async function expectClosed(page: Page) {
  await expect.poll(() => isOpen(page), { message: "the picker is closed" }).toBe(false);
  await expect(picker(page)).toBeHidden();
  await expect.poll(() => expandedRows(page)).toEqual([]);
}

/// Open `row`'s picker from the keyboard: focus its React, Enter.
async function openFromKeyboard(page: Page, row: Locator) {
  await page.mouse.move(0, 0);
  await row.getByTestId("message-react-button").focus();
  await page.keyboard.press("Enter");
  await expectOpenFor(page, row);
}

/// Open `row`'s picker with a pointer press on its React.
async function openWithPointer(page: Page, row: Locator) {
  await revealFooter(page, row);
  const react = row.getByTestId("message-react-button");
  if (await coarsePointer(page)) await react.tap();
  else await react.click();
  await expectOpenFor(page, row);
}

test.describe("The reaction picker", () => {
  test("is one native popover: Escape and a press outside close it, and the press still lands", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "native other");
    await deliver(page, "native target");
    // Room below the target, so the picker opens down from it, clear of the
    // message above.
    for (let i = 0; i < 3; i++) await deliver(page, `native filler ${i}: ${"n".repeat(200)}`);
    const [row, other] = [committedRow(page, "native target"), committedRow(page, "native other")];
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));

    await expect(picker(page), "one picker, mounted closed").toHaveCount(1);
    await expect(picker(page)).toHaveAttribute("popover", "auto");
    await expect(picker(page)).toBeHidden();

    await openFromKeyboard(page, row);
    await expect(picker(page)).toHaveCount(1);
    await expect(row.getByTestId("message-react-button"), "a key press leaves focus on React").toBeFocused();
    // Enter again keeps it open, as it always has.
    await page.keyboard.press("Enter");
    await nextFrames(page);
    await expectOpenFor(page, row);
    await page.keyboard.press("Escape");
    await expectClosed(page);
    await expect(row.getByTestId("message-react-button")).toBeFocused();

    // A press outside closes it, and reaches what it pressed: another
    // message's Reply starts a reply to that message on that same press, and
    // nothing is reacted.
    await openWithPointer(page, row);
    await revealFooter(page, other);
    const box = (await other.getByTestId("message-reply-button").boundingBox())!;
    const [x, y] = [box.x + box.width / 2, box.y + box.height / 2];
    const landsOn = () =>
      page.evaluate(([x, y]) => {
        const b = document.elementFromPoint(x, y)?.closest("button");
        return b ? `${b.getAttribute("data-testid")} of ${b.closest('[id^="msg-"]')?.id}` : null;
      }, [x, y]);
    await expect
      .poll(landsOn, { message: "premise: the other message's Reply is uncovered" })
      .toBe(`message-reply-button of ${await other.getAttribute("id")}`);
    expect(await isOpen(page), "premise: still open").toBe(true);
    if (await coarsePointer(page)) await page.touchscreen.tap(x, y);
    else await page.mouse.click(x, y);
    await expectClosed(page);
    await expect(page.getByTestId("message-composer")).toContainText("Replying to");
    await expect(page.getByTestId("message-composer")).toContainText("native other");
    for (const r of [row, other]) await expect(r.getByTestId("reaction-chip"), "nothing was reacted").toHaveCount(0);

    // A press on the React it is open for closes it.
    await openWithPointer(page, row);
    const react = row.getByTestId("message-react-button");
    if (await coarsePointer(page)) await react.tap();
    else await react.click();
    await expectClosed(page);
  });

  test("a pick reacts once on its message, closes, and returns focus to React without scrolling", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "pick target");
    const row = committedRow(page, "pick target");
    await expect(row.getByTestId("reaction-chip")).toHaveCount(0);
    await readerScrollsWithoutGesture(page, (await scrollMetrics(page)).top - 40);

    // Read after focusing React, which Playwright's focus scrolls to.
    await openFromKeyboard(page, row);
    const parked = (await scrollMetrics(page)).top;
    const first = picker(page).getByRole("button").first();
    const emoji = (await first.textContent())!.trim();
    await first.focus();
    await page.keyboard.press("Enter");
    await expectClosed(page);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);
    await expect(row.getByTestId("reaction-chip")).toContainText(emoji);
    await expect(row.getByTestId("message-react-button"), "focus goes back to React").toBeFocused();
    await settle(page);
    expect((await scrollMetrics(page)).top, "nothing scrolled").toBeCloseTo(parked, 0);

    // Reopened, the reader's reaction is the highlighted one.
    await openFromKeyboard(page, row);
    await expect(picker(page).getByTitle(`Remove ${emoji} reaction`)).toHaveCount(1);
    await page.keyboard.press("Escape");
  });

  // The seeded reactions are signed reaction messages, so the rebuild a new
  // reaction triggers keeps them. They once lived only in the derived cache,
  // and every one vanished on the first pick. One reaction per member per
  // message: the reader's own 👍 becomes the pick, another member's ❤️ stays.
  test("a pick on a message that already has reactions keeps the others' reactions", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    const row = page
      .getByTestId("conversation-history")
      .getByTestId("message-row")
      .filter({ has: page.getByTestId("reaction-chip").filter({ hasText: "❤️" }) });
    await expect(row, "premise: one seeded message carries 👍 and ❤️").toHaveCount(1);
    const chips = row.getByTestId("reaction-chip");
    await expect(chips).toHaveCount(2);
    await expect(chips.filter({ hasText: "👍" }), "premise: the reader's own 👍").toHaveCount(1);

    await row.scrollIntoViewIfNeeded();
    await revealFooter(page, row);
    await row.getByTestId("message-react-button").click();
    const fresh = picker(page).getByRole("button").filter({ hasNotText: /👍|❤️/ }).first();
    const emoji = (await fresh.textContent())!.trim();
    await fresh.click();

    await expect(chips.filter({ hasText: emoji })).toHaveCount(1);
    await expect(chips.filter({ hasText: "❤️" }), "another member's reaction stays").toHaveCount(1);
    await expect(chips.filter({ hasText: "👍" }), "the reader's own reaction is switched").toHaveCount(0);
    await expect(chips).toHaveCount(2);
    // Another seeded message's reaction is untouched too.
    await expect(page.getByTestId("reaction-chip").filter({ hasText: "🎉" })).toHaveCount(1);
  });

  test("opening and closing it renders no message group", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "render target");
    const row = committedRow(page, "render target");
    await settle(page);
    await callRiverTest(page, "takeMessageGroupRenders");
    await openFromKeyboard(page, row);
    await page.keyboard.press("Escape");
    await expectClosed(page);
    await settle(page);
    expect(await callRiverTest(page, "takeMessageGroupRenders"), "the picker's state is not read by the groups").toBe(0);
  });

  test("another React moves it there at once, and a pick acts on that message only", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "rapid one");
    // Far enough apart that the picker at one never covers the other's React.
    for (let i = 0; i < 2; i++) await deliver(page, `rapid filler ${i}: ${"r".repeat(300)}`);
    await deliver(page, "rapid two");
    const [one, two] = [committedRow(page, "rapid one"), committedRow(page, "rapid two")];

    // A press on another React while it is open.
    await openWithPointer(page, one);
    await openWithPointer(page, two);
    expect(placement(await measurePicker(two)).block).not.toContain("off");

    // Two activations in one task, as fast as anything can: the later wins,
    // and the earlier opening's close does not undo it.
    await page.keyboard.press("Escape");
    await expectClosed(page);
    await page.evaluate(() => {
      const reacts = Array.from(document.querySelectorAll<HTMLElement>('[data-testid="message-react-button"]'));
      const byText = (t: string) => reacts.find((b) => b.closest('[id^="msg-"]')!.textContent!.includes(t))!;
      byText("rapid one").click();
      byText("rapid two").click();
    });
    await settle(page);
    await expectOpenFor(page, two);
    await picker(page).getByRole("button").first().click();
    await expectClosed(page);
    await expect(two.getByTestId("reaction-chip")).toHaveCount(1);
    await expect(one.getByTestId("reaction-chip")).toHaveCount(0);
  });

  test("a room switch closes it, and a pick queued across the switch does nothing", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "switch target");
    const row = committedRow(page, "switch target");
    const id = (await row.getAttribute("id"))!;

    // From the keyboard, where nothing presses outside the picker.
    await openFromKeyboard(page, row);
    if (!(await coarsePointer(page))) {
      const item = page.getByTestId("room-list").getByRole("button", { name: "Team Chat Room" });
      await item.focus();
      await page.keyboard.press("Enter");
    } else {
      await callRiverTest(page, "switchRoom", "Team Chat Room");
    }
    await expect(page.locator(`[id="${id}"]`)).toHaveCount(0);
    await expectClosed(page);

    // Back: nothing open, and a fresh opening works.
    await callRiverTest(page, "switchRoom", "Your Private Room");
    await expect(row).toHaveCount(1);
    await expectClosed(page);
    await openFromKeyboard(page, row);

    // A pick and a switch in one task, the switch first: the pick runs after
    // it and must not react, here or in the room it was made in.
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await page.evaluate((sel) => {
      const hooks = (window as unknown as { __riverTest: { switchRoom(n: string): void } }).__riverTest;
      hooks.switchRoom("Team Chat Room");
      document.querySelector<HTMLElement>(`${sel} button`)!.click();
    }, PICKER);
    await expect(page.locator(`[id="${id}"]`)).toHaveCount(0);
    await settle(page);
    await expectClosed(page);
    expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signs);
    await callRiverTest(page, "switchRoom", "Your Private Room");
    await expect(row).toHaveCount(1);
    await expect(row.getByTestId("reaction-chip"), "no reaction in the room it was made in").toHaveCount(0);

    // Away and back before the pick runs: the message is on screen again, but
    // in a new showing of the room, which the pick was not made in.
    await openFromKeyboard(page, row);
    await page.evaluate((sel) => {
      const hooks = (window as unknown as { __riverTest: { switchRoom(n: string): void } }).__riverTest;
      hooks.switchRoom("Team Chat Room");
      hooks.switchRoom("Your Private Room");
      document.querySelector<HTMLElement>(`${sel} button`)!.click();
    }, PICKER);
    await settle(page);
    await expect(row).toHaveCount(1);
    await expectClosed(page);
    expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signs);
    await expect(row.getByTestId("reaction-chip"), "no reaction from the earlier showing").toHaveCount(0);
  });

  test("a message deleted while its picker is open takes the picker with it, and a rollback does not reopen it", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "doomed target");
    await expect(row).toHaveCount(1);
    await callRiverTest(page, "holdMessageSigning");
    await openFromKeyboard(page, row);
    const signs = await callRiverTest(page, "messageSignsRecorded");

    // Delete it from its own footer and confirm, without a press outside the
    // picker; in the same task, a pick on it.
    await row.getByTestId("message-delete-button").evaluate((b) => (b as HTMLElement).click());
    const confirm = page.getByRole("button", { name: "Delete", exact: true });
    await expect(confirm).toBeVisible();
    await page.evaluate((sel) => {
      const del = Array.from(document.querySelectorAll<HTMLElement>("button")).find((b) => b.textContent!.trim() === "Delete")!;
      del.click();
      document.querySelector<HTMLElement>(`${sel} button`)!.click();
    }, PICKER);
    await expect(row, "the delete shows at once").toHaveCount(0);
    await expectClosed(page);

    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(row, "the failed delete is rolled back").toHaveCount(1);
    await settle(page);
    await expectClosed(page);
    await expect(row.getByTestId("reaction-chip"), "the queued pick was not replayed").toHaveCount(0);
    expect(await callRiverTest(page, "messageSignsRecorded"), "only the delete was signed").toBe(signs + 1);
  });

  // A group is keyed by its first message, so removing that message remounts
  // every row after it: the target's row comes back as a new element with a
  // new React. The picker stays open for it, hangs from the new React, keeps
  // the focus it had (inside the picker, or on React), is what Tab and a pick
  // return to, and the input hold stays on.
  test("its message's group remounting keeps it open at the new React, and a pick acts once there", async ({ page, browserName }) => {
    // WebKit's Tab reaches only text fields; Alt+Tab reaches every control.
    const [tab, backTab] = browserName === "webkit" ? ["Alt+Tab", "Alt+Shift+Tab"] : ["Tab", "Shift+Tab"];
    await openRoomWithComposer(page);
    for (let i = 0; i < 4; i++) await deliver(page, `remount filler ${i}: ${"m".repeat(200)}`);
    // Stamped just ahead of the fillers, so the run is the newest group: a
    // group sorted above later messages is moved by the next render, which
    // loses focus on its own.
    await callRiverTest(page, "deliverTimedMessages", 0, [
      { text: "remount lead", sentSecs: 1, receivedSecs: 0 },
      { text: "remount middle", sentSecs: 2, receivedSecs: 0 },
      { text: "remount survivor", sentSecs: 3, receivedSecs: 0 },
    ]);
    const [lead, middle, survivor] = ["remount lead", "remount middle", "remount survivor"].map((t) => committedRow(page, t));
    await expect(survivor).toHaveCount(1);
    const [leadKey, middleKey, survivorKey] = [await rowKey(lead), await rowKey(middle), await rowKey(survivor)];
    const groupKey = (row: Locator) => row.evaluate((el) => el.closest(".msg-group")?.getAttribute("data-item-key") ?? null);
    for (const row of [lead, middle, survivor]) expect(await groupKey(row), "premise: one group, keyed by its first message").toBe(leadKey);
    expect(
      await survivor.evaluate((el) => !el.closest(".msg-group")!.nextElementSibling?.querySelector('[data-testid="message-row"]')),
      "premise: the newest group",
    ).toBe(true);

    const emoji = picker(page).getByRole("button").first();
    const liveReactFocused = () =>
      survivor.evaluate((el) => {
        const react = el.querySelector('[data-testid="message-react-button"]');
        return !!react && react.isConnected && document.activeElement === react;
      });
    /// Remove the group's first message, with no press anywhere, and check
    /// the survivor's row and React came back as new elements.
    const remountBy = async (gone: Locator, text: string, newKey: string) => {
      await survivor.evaluate((el) => {
        const w = window as unknown as { __oldRow: Element; __oldReact: Element };
        w.__oldRow = el;
        w.__oldReact = el.querySelector('[data-testid="message-react-button"]')!;
      });
      await callRiverTest(page, "removeMessages", text);
      await expect(gone).toHaveCount(0);
      await expect.poll(() => groupKey(survivor), { message: "the group is keyed by its new first message" }).toBe(newKey);
      await settle(page);
      const replaced = await survivor.evaluate((el) => {
        const w = window as unknown as { __oldRow: Element; __oldReact: Element };
        return { sameRow: el === w.__oldRow, oldConnected: w.__oldReact.isConnected };
      });
      expect(replaced, "premise: the survivor's row and React are new elements").toEqual({ sameRow: false, oldConnected: false });
      await expectOpenFor(page, survivor);
      const m = await measurePicker(survivor);
      expectAtReact(m, "placed at the new React");
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
    };

    await openFromKeyboard(page, survivor);
    // Into the picker from React: the picker follows React in the tab order.
    await page.keyboard.press(tab);
    await expect(emoji, "premise: Tab from React enters the picker").toBeFocused();
    const signs = await callRiverTest(page, "messageSignsRecorded");

    // Focus inside the picker stays there.
    await remountBy(lead, "remount lead", middleKey);
    await expect(emoji, "focus stays on the emoji it was on").toBeFocused();
    await page.keyboard.press(backTab);
    expect(await liveReactFocused(), "back out of the picker: the new React").toBe(true);

    // Focus on React goes to the new React.
    await remountBy(middle, "remount middle", survivorKey);
    expect(await liveReactFocused(), "focus moves to the new React").toBe(true);
    await page.keyboard.press(tab);
    await expect(emoji, "and Tab from it enters the picker").toBeFocused();

    // Still held: a wheel over the history moves nothing.
    if (!(await coarsePointer(page))) {
      const view = (await page.locator("#chat-scroll-container").boundingBox())!;
      const [x, y] = [view.x + 30, view.y + 30];
      expect(await hitAt(page, x, y), "premise: the history point hits the history").toBe("history");
      expect((await scrollMetrics(page)).top, "premise: room for the wheel").toBeGreaterThan(WHEEL_DELTA + 20);
      const before = (await scrollMetrics(page)).top;
      await wheelAt(page, x, y);
      expect((await scrollMetrics(page)).top, "the wheel is still held").toBe(before);
      await expectOpenFor(page, survivor);
    }

    const parked = (await scrollMetrics(page)).top;
    const picked = (await emoji.textContent())!.trim();
    await page.keyboard.press("Enter");
    await expectClosed(page);
    await expect(survivor.getByTestId("reaction-chip")).toHaveCount(1);
    await expect(survivor.getByTestId("reaction-chip")).toContainText(picked);
    await settle(page);
    expect(await callRiverTest(page, "messageSignsRecorded"), "one pick, signed once").toBe(signs + 1);
    expect(await liveReactFocused(), "focus goes back to the new React").toBe(true);
    expect((await scrollMetrics(page)).top, "nothing scrolled").toBeCloseTo(parked, 0);
  });

  // After a mouse pick, focus goes back to React from script, which is not
  // keyboard focus: React must not then be a focused control nobody can see.
  // It stays shown and usable until focus leaves it, with no ring; the row's
  // other actions keep their hover reveal.
  test("a mouse pick leaves the React it returns focus to shown and usable after the pointer leaves", async ({ page }) => {
    test.skip(await coarsePointer(page), "a touch pointer shows the controls at rest");
    await openRoomWithComposer(page);
    await deliver(page, "mouse focus target");
    const row = committedRow(page, "mouse focus target");
    const react = row.getByTestId("message-react-button");
    await openWithPointer(page, row);
    const parked = (await scrollMetrics(page)).top;
    await picker(page).getByRole("button").first().click();
    await expectClosed(page);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);

    await page.mouse.move(0, 0);
    await settle(page);
    expect(await row.evaluate((el) => el.matches(":hover")), "premise: the row is not hovered").toBe(false);
    const state = (testid: string) =>
      row.evaluate(async (el, testid) => {
        const b = el.querySelector(`[data-testid="${testid}"]`) as HTMLElement;
        await Promise.all(b.getAnimations().map((a) => a.finished));
        const r = b.getBoundingClientRect();
        const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return {
          focused: document.activeElement === b,
          keyboardFocus: b.matches(":focus-visible"),
          opacity: getComputedStyle(b).opacity,
          hit: !!at && b.contains(at),
        };
      }, testid);
    const returned = await state("message-react-button");
    test.info().annotations.push({ type: "focus after a mouse pick", description: returned.focused ? "on React" : "not on React" });
    expect((await scrollMetrics(page)).top, "nothing scrolled").toBeCloseTo(parked, 0);
    if (!returned.focused) {
      // WebKit focuses no button on a click, so no focus is returned; React
      // keeps the hover reveal.
      expect(returned, "unfocused, React is hidden at rest").toEqual({ focused: false, keyboardFocus: false, opacity: "0", hit: false });
      return;
    }
    expect(returned, "focused React is shown and takes hits, without a ring").toEqual({ focused: true, keyboardFocus: false, opacity: "1", hit: true });
    expect(await state("message-reply-button"), "the other actions are not revealed by it").toMatchObject({ opacity: "0", hit: false });
    const height = () => row.evaluate((el) => el.getBoundingClientRect().height);
    const shownHeight = await height();
    await revealFooter(page, row);
    expect(await height(), "showing it moved nothing").toBe(shownHeight);
    await page.mouse.move(0, 0);

    // It is the React it looks like: Enter opens its picker again.
    await page.keyboard.press("Enter");
    await expectOpenFor(page, row);
    await page.keyboard.press("Escape");
    await expectClosed(page);
  });

  // On a long message React is at its foot, after the body, and the picker
  // hangs from it there.
  test("a reaction arriving while it is open moves React, and the picker with it", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "appendMessage", Array.from({ length: 6 }, (_, i) => `reflow line ${i}`).join("\n\n"));
    const row = committedRow(page, "reflow line 5");
    await expect(row).toHaveCount(1);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(0);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await openFromKeyboard(page, row);
    const before = await measurePicker(row);
    const top = await row.evaluate((el) => el.getBoundingClientRect().top);
    expect(before.react.top - top, "premise: a long body above React").toBeGreaterThan(100);
    expectAtReact(before);
    expectEveryEmojiUsable(before);
    expectInvokerUncovered(before);
    await callRiverTest(page, "addReactionsToNewest", ["🎉", "👀", "🔥"]);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(3);
    await nextFrames(page);
    const after = await measurePicker(row);
    expect(after.react.left - before.react.left, "premise: the chips moved React").toBeGreaterThan(20);
    expect(placement(after)).toEqual(placement(before));
    await expectOpenFor(page, row);
  });
});

test.describe("Reaction picker placement", () => {
  // The narrowest window the picker has (below 48rem a React opens the
  // mobile popup). React is never at the window's left edge there, so the
  // picker opens back from it, its right edge on React's.
  test.describe("at 768px", () => {
    test.use({ viewport: { width: 768, height: 700 } });

    test("at a React with no chips it opens back from React, at one at the column's end too, and after wrapped chips at React", async ({ page }) => {
      await openRoomWithComposer(page);
      await deliverOffscreen(page, "start edge");
      // Room below it, so only the inline side has to give.
      for (let i = 0; i < 3; i++) await deliverOffscreen(page, `start filler ${i}: ${"s".repeat(200)}`);
      const start = committedRow(page, "start edge");
      await start.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await openFromKeyboard(page, start);
      let m = await measurePicker(start);
      expect(m.react.right - m.react.left, "premise").toBeGreaterThan(0);
      expect(m.react.right - (m.picker.right - m.picker.left), "premise: room back from React").toBeGreaterThan(0);
      expect(placement(m)).toEqual({ block: "below", inline: "end" });
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
      await page.keyboard.press("Escape");

      // As many chips as fit one line put the newest message's React at the
      // column's right edge: it opens back from React, its right edge on
      // React's. The count is measured: a fixed one fits a line in one engine
      // and wraps in another (WebKit's emoji chips are 3px wider).
      const emojis = ["👍", "❤️", "😂", "🔥", "🎉", "👀", "😮", "😢", "🙏", "💯", "🦀", "🐙", "🌵", "🍕", "🚀", "⚡", "🎈", "🧩", "🎯", "🪐", "🌈", "🍀", "🐝", "🦄"];
      const end = committedRow(page, "start filler 2");
      const chips = end.getByTestId("reaction-chip");
      await callRiverTest(page, "addReactionsToNewest", emojis.slice(0, 1));
      await expect(chips).toHaveCount(1);
      const fit = await end.evaluate((el) => {
        const chip = el.querySelector('[data-testid="reaction-chip"]')!.getBoundingClientRect();
        const react = el.querySelector('[data-testid="message-react-button"]')!.getBoundingClientRect();
        const footer = el.querySelector('[data-testid="message-footer"]')!.getBoundingClientRect();
        // Each chip moves React one chip and its gap further right.
        return Math.floor((footer.right - react.right) / (react.left - chip.left)) + 1;
      });
      expect(fit, "chips that fit one line").toBeLessThanOrEqual(emojis.length);
      await callRiverTest(page, "addReactionsToNewest", emojis.slice(1, fit));
      await expect(chips).toHaveCount(fit);
      await end.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await openFromKeyboard(page, end);
      m = await measurePicker(end);
      const footerRight = await end.evaluate((el) => el.querySelector('[data-testid="message-footer"]')!.getBoundingClientRect().right);
      expect(footerRight - m.react.right, "premise: React at the column's right edge").toBeLessThan(60);
      expect(m.react.right - (m.picker.right - m.picker.left), "premise: room back from React").toBeGreaterThan(0);
      expect(placement(m).inline).toBe("end");
      expect(["below", "above"]).toContain(placement(m).block);
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
      await page.keyboard.press("Escape");
      await expectClosed(page);

      // Chips that wrap onto several lines: React follows them, and the
      // picker hangs from it there.
      await callRiverTest(page, "appendMessage", "wrap target");
      const wrapped = committedRow(page, "wrap target");
      await expect(wrapped).toHaveCount(1);
      await callRiverTest(page, "addReactionsToNewest", emojis);
      await expect(wrapped.getByTestId("reaction-chip")).toHaveCount(emojis.length);
      await wrapped.evaluate((el) => el.scrollIntoView({ block: "end" }));
      const lines = await wrapped.getByTestId("reaction-chip").evaluateAll((els) => new Set(els.map((e) => Math.round(e.getBoundingClientRect().top))).size);
      expect(lines, "premise: the chips wrap").toBeGreaterThan(1);
      await openFromKeyboard(page, wrapped);
      m = await measurePicker(wrapped);
      expectAtReact(m);
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
    });
  });

  // React at the window's left edge, with no room back from it, is no
  // longer reachable by layout at 48rem and wider (the rooms rail is there);
  // the rail is taken out here so the start-side fallback stays covered.
  test.describe("with no room back from React", () => {
    test.use({ viewport: { width: 900, height: 700 } });

    test("it opens from React's left edge toward the free side", async ({ page }) => {
      await openRoomWithComposer(page);
      // The rooms rail out, and the column against the window's left edge.
      await page.evaluate(() => {
        let rail: Element | null = document.querySelector('[data-testid="room-list"]');
        while (rail && rail.parentElement && !rail.parentElement.classList.contains("app-root")) rail = rail.parentElement;
        (rail as HTMLElement).style.setProperty("display", "none", "important");
      });
      await page.addStyleTag({ content: ".conversation-column { margin-inline: 0 !important; }" });
      await deliverOffscreen(page, "start edge desktop");
      for (let i = 0; i < 3; i++) await deliverOffscreen(page, `start desktop filler ${i}: ${"s".repeat(200)}`);
      const start = committedRow(page, "start edge desktop");
      await start.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await openFromKeyboard(page, start);
      const m = await measurePicker(start);
      expect(m.react.right - (m.picker.right - m.picker.left), "premise: no room back from React").toBeLessThan(0);
      expect(placement(m)).toEqual({ block: "below", inline: "start" });
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
    });
  });

  test.describe("in a short landscape view", () => {
    test.use({ viewport: { width: 800, height: 360 } });

    test("at the newest message's React it opens above, back from React", async ({ page }) => {
      await openRoomWithComposer(page);
      await deliver(page, "corner target");
      const row = committedRow(page, "corner target");
      await openFromKeyboard(page, row);
      const m = await measurePicker(row);
      expect(m.vh - m.react.bottom, "premise: no room below React").toBeLessThan(m.picker.bottom - m.picker.top + PICKER_GAP);
      expect(placement(m)).toEqual({ block: "above", inline: "end" });
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
    });
  });

  // Simulated: every engine this suite runs supports anchor positioning, so
  // the rule is taken out of the stylesheet.
  test("without anchor positioning it opens centred in the viewport, and still works", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "fallback target");
    const row = committedRow(page, "fallback target");
    const removed = await page.evaluate(() => {
      let n = 0;
      for (const sheet of Array.from(document.styleSheets)) {
        let rules: CSSRuleList;
        try {
          rules = sheet.cssRules;
        } catch {
          continue;
        }
        for (let i = rules.length - 1; i >= 0; i--) {
          const r = rules[i];
          if (r instanceof CSSSupportsRule && r.conditionText.includes("position-area") && r.cssText.includes(".reaction-picker")) {
            sheet.deleteRule(i);
            n++;
          }
        }
      }
      return n;
    });
    expect(removed, "premise: the anchored rule was found").toBe(1);
    await openFromKeyboard(page, row);
    const m = await measurePicker(row);
    expect(Math.abs(m.picker.left - (m.vw - m.picker.right)), "centred across").toBeLessThanOrEqual(1);
    expect(Math.abs(m.picker.top - (m.vh - m.picker.bottom)), "centred down").toBeLessThanOrEqual(1);
    expectEveryEmojiUsable(m);
    await picker(page).getByRole("button").first().click();
    await expectClosed(page);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);
  });
});

const WHEEL_DELTA = 200;

/// Whether `row`'s React has no part inside the history's visible box.
function reactOutOfView(page: Page, row: Locator): Promise<boolean> {
  return row.getByTestId("message-react-button").evaluate((react) => {
    const a = react.getBoundingClientRect();
    const h = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return a.bottom <= h.top || a.top >= h.bottom || a.right <= h.left || a.left >= h.right;
  });
}

/// What a press at (x, y) lands in: the picker, the history, or neither.
function hitAt(page: Page, x: number, y: number): Promise<string> {
  return page.evaluate(
    ([x, y, sel]) => {
      const at = document.elementFromPoint(x, y);
      if (at?.closest(sel)) return "picker";
      if (at?.closest("#chat-scroll-container")) return "history";
      return `elsewhere (${at?.tagName}.${at?.className})`;
    },
    [x, y, PICKER] as const,
  );
}

/// A real wheel upward at (x, y), settled.
async function wheelAt(page: Page, x: number, y: number) {
  await page.mouse.move(x, y);
  await page.mouse.wheel(0, -WHEEL_DELTA);
  await settle(page);
}

test.describe("While the reaction picker is open", () => {
  test("the reader's wheel scrolls the history neither over it nor over the picker; closing it lets it again", async ({ page }) => {
    test.skip(test.info().project.name.startsWith("mobile"), "no wheel on a phone");
    await openRoomWithComposer(page);
    for (let i = 0; i < 6; i++) await deliver(page, `wheel filler ${i}: ${"w".repeat(200)}`);
    const row = committedRow(page, "wheel filler 3");
    const park = async () => {
      await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await settle(page);
      const top = (await scrollMetrics(page)).top;
      expect(top, "premise: room for the whole wheel delta upward").toBeGreaterThan(WHEEL_DELTA + 20);
      return top;
    };

    // Before opening, the same wheel at the same point scrolls the history.
    const parked = await park();
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const [x, y] = [view.x + 30, view.y + 30];
    expect(await hitAt(page, x, y), "premise: the history point hits the history").toBe("history");
    await wheelAt(page, x, y);
    expect((await scrollMetrics(page)).top, "premise: unheld, the wheel scrolls the history").toBeLessThan(parked);

    const top = await park();
    await openWithPointer(page, row);
    const p = (await picker(page).boundingBox())!;
    const [px, py] = [p.x + p.width / 2, p.y + p.height / 2];
    expect(await hitAt(page, x, y), "premise: the history point is not under the picker").toBe("history");
    expect(await hitAt(page, px, py), "premise: the picker point hits the picker").toBe("picker");
    expect((await scrollMetrics(page)).top, "premise: opening scrolled nothing").toBe(top);
    for (const [where, wx, wy] of [
      ["over the history", x, y],
      ["over the picker", px, py],
    ] as const) {
      const before = (await scrollMetrics(page)).top;
      await wheelAt(page, wx, wy);
      expect((await scrollMetrics(page)).top, `${where}: the history did not move`).toBe(before);
      expect(await isOpen(page), `${where}: still open`).toBe(true);
    }

    await page.keyboard.press("Escape");
    await expectClosed(page);
    const before = (await scrollMetrics(page)).top;
    await wheelAt(page, x, y);
    expect((await scrollMetrics(page)).top, "closed: the wheel scrolls").toBeLessThan(before);
  });

  // The handler's contract, not proof of a zoom: a plain wheel is cancelled
  // wherever it lands, a Ctrl wheel (Ctrl+wheel, a touchpad pinch) is not.
  test("a wheel with Ctrl down is a zoom and is let through, over the history and over the picker", async ({ page }) => {
    await openRoomWithComposer(page);
    for (let i = 0; i < 6; i++) await deliver(page, `zoom filler ${i}: ${"z".repeat(200)}`);
    const row = committedRow(page, "zoom filler 3");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await settle(page);
    await openFromKeyboard(page, row);
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const p = (await picker(page).boundingBox())!;
    const points = {
      history: [view.x + 30, view.y + 30],
      picker: [p.x + p.width / 2, p.y + p.height / 2],
    } as const;
    for (const [where, [x, y]] of Object.entries(points)) {
      expect(await hitAt(page, x, y), `premise: the ${where} point hits the ${where}`).toBe(where);
      expect(await wheelCancelled(page, x, y, false), `${where}: a plain wheel is held`).toBe(true);
      expect(await wheelCancelled(page, x, y, true), `${where}: a Ctrl wheel is let through`).toBe(false);
    }
    expect(await isOpen(page), "still open").toBe(true);

    await page.keyboard.press("Escape");
    await expectClosed(page);
    const [x, y] = points.history;
    expect(await wheelCancelled(page, x, y, false), "closed: a plain wheel is let through").toBe(false);
  });

  test("a touchpad pinch over the history still zooms", async ({ page, browserName }) => {
    test.skip(browserName !== "chromium" || test.info().project.name.startsWith("mobile"), "a touchpad pinch needs Chromium's input protocol and a desktop");
    await openRoomWithComposer(page);
    await deliver(page, "pinch anchor");
    const row = committedRow(page, "pinch anchor");
    await openFromKeyboard(page, row);
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const [x, y] = [Math.round(view.x + 100), Math.round(view.y + 40)];
    expect(await hitAt(page, x, y), "premise: the pinch is over the history").toBe("history");
    const cdp = await page.context().newCDPSession(page);
    try {
      // A touchpad pinch reaches the page as wheel events with Ctrl down.
      await cdp.send("Input.synthesizePinchGesture", { x, y, scaleFactor: 2, gestureSourceType: "mouse" });
    } finally {
      await cdp.detach();
    }
    await settle(page);
    expect(await page.evaluate(() => window.visualViewport!.scale), "the page zoomed").toBeGreaterThan(1.5);
  });

  test("an arrival is still followed", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "follow anchor");
    const row = committedRow(page, "follow anchor");
    await openFromKeyboard(page, row);
    await deliver(page, "followed while open");
    await expectFollowed(page, "followed while open", "an arrival follows under the open picker");
    await expectOpenFor(page, row);
  });

  test("once arrivals carry its React out of the history's view, the next wheel closes it and scrolls", async ({ page }) => {
    test.skip(test.info().project.name.startsWith("mobile"), "no wheel on a phone");
    await openRoomWithComposer(page);
    await deliver(page, "carried anchor");
    const row = committedRow(page, "carried anchor");
    await openFromKeyboard(page, row);
    for (let i = 0; i < 30 && !(await reactOutOfView(page, row)); i++) {
      await deliver(page, `carried filler ${i}: ${"c".repeat(200)}`);
    }
    expect(await reactOutOfView(page, row), "premise: arrivals carried React out of the history's view").toBe(true);
    expect(await isOpen(page), "premise: arrivals alone do not close it").toBe(true);

    // Low in the history: an engine that keeps the picker in the viewport
    // (rather than following React out) leaves it near the top.
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const [x, y] = [view.x + 30, view.y + view.height - 40];
    expect(await hitAt(page, x, y), "premise: the point hits the history").toBe("history");
    const before = (await scrollMetrics(page)).top;
    await wheelAt(page, x, y);
    expect((await scrollMetrics(page)).top, "the wheel scrolled the history").toBeLessThan(before);
    await expectClosed(page);
  });

  test("a React only partly inside the history's view still holds the wheel", async ({ page }) => {
    test.skip(test.info().project.name.startsWith("mobile"), "no wheel on a phone");
    await openRoomWithComposer(page);
    for (let i = 0; i < 6; i++) await deliver(page, `clipped filler ${i}: ${"p".repeat(200)}`);
    const row = committedRow(page, "clipped filler 3");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await settle(page);
    await openFromKeyboard(page, row);
    // Programmatic scrolling is not held: put React half below the bottom edge.
    await row.getByTestId("message-react-button").evaluate((react) => {
      const c = document.getElementById("chat-scroll-container")!;
      const a = react.getBoundingClientRect();
      c.scrollTop += a.top - (c.getBoundingClientRect().bottom - a.height / 2);
    });
    await settle(page);
    const straddles = await row.getByTestId("message-react-button").evaluate((react) => {
      const a = react.getBoundingClientRect();
      const bottom = document.getElementById("chat-scroll-container")!.getBoundingClientRect().bottom;
      return a.top < bottom && a.bottom > bottom;
    });
    expect(straddles, "premise: React straddles the history's bottom edge").toBe(true);
    expect(await isOpen(page), "premise: still open").toBe(true);

    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const [x, y] = [view.x + 30, view.y + 30];
    expect(await hitAt(page, x, y), "premise: the point hits the history").toBe("history");
    const before = (await scrollMetrics(page)).top;
    await wheelAt(page, x, y);
    expect((await scrollMetrics(page)).top, "held: the history did not move").toBe(before);
    expect(await isOpen(page), "still open").toBe(true);
  });

  // #863 with a desktop width: its React stops being laid out (its row
  // collapsed here; the narrow-window version of this now closes the picker
  // at 48rem instead). A zoom is let through and closes nothing; the next
  // plain wheel is let through and closes it.
  test("once its React is no longer laid out, a zoom closes nothing and the next wheel is let through and closes it", async ({ page }) => {
    test.skip(test.info().project.name.startsWith("mobile"), "no wheel on a phone");
    await page.setViewportSize({ width: 1280, height: 844 });
    await openRoomWithComposer(page);
    await deliver(page, "collapsed anchor");
    const row = committedRow(page, "collapsed anchor");
    await openFromKeyboard(page, row);
    await page.addStyleTag({ content: '.msg-group:has([data-testid="message-react-button"][aria-expanded="true"]) { display: none !important; }' });
    await nextFrames(page);
    expect(await row.evaluate((el) => el.getBoundingClientRect().height), "premise: its row is not laid out").toBe(0);
    expect(await isOpen(page), "premise: hiding it alone does not close it").toBe(true);
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const [x, y] = [view.x + 30, view.y + 30];
    expect(await wheelCancelled(page, x, y, true), "a zoom is let through").toBe(false);
    expect(await isOpen(page), "a zoom does not close it").toBe(true);
    expect(await wheelCancelled(page, x, y, false), "the wheel is not cancelled").toBe(false);
    await expectClosed(page);
  });

  // Below 48rem the React is the mobile popup's, so narrowing the window
  // closes the picker outright and lets go of its hold; widening again
  // reopens nothing.
  test("narrowing the window below 48rem closes it, lets go of the wheel, and widening reopens nothing", { tag: "@chromium-only" }, async ({ page }) => {
    test.skip(test.info().project.name.startsWith("mobile"), "no wheel on a phone");
    await page.setViewportSize({ width: 1280, height: 844 });
    await openRoomWithComposer(page);
    await deliver(page, "narrowed anchor");
    const row = committedRow(page, "narrowed anchor");
    await openFromKeyboard(page, row);

    await page.setViewportSize({ width: 390, height: 844 });
    await expectClosed(page);
    await expect(row.getByTestId("message-react-button"), "now the popup's").toHaveAttribute("aria-controls", "mobile-reaction-popup");
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const [x, y] = [view.x + 30, view.y + 30];
    expect(await wheelCancelled(page, x, y, false), "the wheel is not held").toBe(false);
    await page.setViewportSize({ width: 1280, height: 844 });
    await settle(page);
    await expectClosed(page);
    await expect(row.getByTestId("message-react-button")).toHaveAttribute("aria-controls", "reaction-picker");
  });

  test("an automatic reload waits, and closing it does not release one an unsent message holds", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "reload target");
    const row = committedRow(page, "reload target");
    await page.getByTestId("message-input").blur();
    expect(await callRiverTest(page, "userIsBusy"), "premise: idle").toBe(false);

    await openFromKeyboard(page, row);
    expect(await callRiverTest(page, "userIsBusy"), "busy while it is open").toBe(true);
    await page.keyboard.press("Escape");
    await expectClosed(page);
    await page.mouse.move(0, 0);
    await row.getByTestId("message-react-button").blur();
    expect(await callRiverTest(page, "userIsBusy"), "closed and mounted: not busy").toBe(false);

    // A message still being signed holds a reload whether or not the picker
    // is open, and closing the picker does not let it go.
    await callRiverTest(page, "holdMessageSigning");
    const held = await submitMessage(page, "held while picking");
    await expect(page.getByTestId("pending-message").filter({ hasText: "held while picking" }), "premise: still being signed").toHaveCount(1);
    await openFromKeyboard(page, row);
    await page.keyboard.press("Escape");
    await expectClosed(page);
    await row.getByTestId("message-react-button").blur();
    expect(await callRiverTest(page, "userIsBusy"), "the unsent message still holds it").toBe(true);
    await callRiverTest(page, "releaseMessageSigning");
    await expect(held).toHaveCount(1);
    await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "signed: released" }).toBe(false);
  });

  // The app toast and the picker can meet: the picker paints over the toast
  // where they overlap, and a press on an uncovered toast button closes the
  // picker and runs the button on that same press.
  test("an uncovered toast action runs once on the press that closes the picker; a covered one waits", async ({ page }) => {
    await page.setViewportSize({ width: 768, height: 700 });
    await openRoomWithComposer(page);
    await deliver(page, "toast target");
    const row = committedRow(page, "toast target");
    await callRiverTest(page, "takeToastActionRuns");

    await openFromKeyboard(page, row);
    await callRiverTest(page, "showToast", "Thread archived", "Undo");
    const action = page.getByTestId("toast-action");
    await expect(action).toBeVisible();
    const box = (await action.boundingBox())!;
    const hit = await page.evaluate(([x, y]) => document.elementFromPoint(x, y)?.getAttribute("data-testid") ?? null, [box.x + box.width / 2, box.y + box.height / 2]);
    expect(hit, "premise: the action is uncovered").toBe("toast-action");
    if (await coarsePointer(page)) await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);
    else await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expectClosed(page);
    expect(await callRiverTest(page, "takeToastActionRuns"), "the action ran once, on that press").toBe(1);
    await expect(action).toHaveCount(0);

    // Covered: move the toast under the open picker. The press lands on the
    // picker; the action is still there once the picker is dismissed.
    await openFromKeyboard(page, row);
    await callRiverTest(page, "showToast", "Thread archived", "Undo");
    await expect(action).toBeVisible();
    const [p, a] = [(await picker(page).boundingBox())!, (await action.boundingBox())!];
    const [dx, dy] = [p.x + p.width / 2 - (a.x + a.width / 2), p.y + p.height / 2 - (a.y + a.height / 2)];
    await page.addStyleTag({ content: `.app-toast-host { transform: translate(${Math.round(dx)}px, ${Math.round(dy)}px); }` });
    const covered = (await action.boundingBox())!;
    const lands = await page.evaluate(([x, y, sel]) => !!document.elementFromPoint(x as number, y as number)?.closest(sel as string), [covered.x + covered.width / 2, covered.y + covered.height / 2, PICKER]);
    expect(lands, "covered: a press there lands on the picker").toBe(true);
    await page.keyboard.press("Escape");
    await expectClosed(page);
    await action.click();
    expect(await callRiverTest(page, "takeToastActionRuns"), "the action ran once, after the picker closed").toBe(1);
  });
});

/// Record the touch pointers pressed and every touchmove as the page last
/// sees it (window, bubbling): how many fingers it had and whether it was
/// cancelled.
function recordTouch(page: Page) {
  return page.evaluate(() => {
    const w = window as unknown as { __touch: { ids: Set<number>; moves: { touches: number; prevented: boolean }[] } };
    w.__touch = { ids: new Set(), moves: [] };
    addEventListener("pointerdown", (e) => e.pointerType === "touch" && w.__touch.ids.add(e.pointerId), { capture: true, passive: true });
    addEventListener("touchmove", (e) => w.__touch.moves.push({ touches: e.touches.length, prevented: e.defaultPrevented }), { passive: true });
  });
}

/// The touch pointers the picker holds captured now.
function pickerCaptures(page: Page): Promise<number[]> {
  return page.evaluate((sel) => {
    const p = document.querySelector(sel)!;
    const w = window as unknown as { __touch: { ids: Set<number> } };
    return [...w.__touch.ids].filter((id) => p.hasPointerCapture(id));
  }, PICKER);
}

/// The touchmoves recorded since the last call.
function takeTouchMoves(page: Page): Promise<{ touches: number; prevented: boolean }[]> {
  return page.evaluate(() => {
    const w = window as unknown as { __touch: { moves: { touches: number; prevented: boolean }[] } };
    return w.__touch.moves.splice(0);
  });
}

async function touchRoom(page: Page, label: string): Promise<Locator> {
  await openRoomWithComposer(page);
  for (let i = 0; i < 6; i++) await deliver(page, `${label} ${i}: ${"t".repeat(200)}`);
  await recordTouch(page);
  return committedRow(page, `${label} 3`);
}

/// Park the history with `row` centred, with room above for the whole drag
/// (a finger moving down scrolls toward older messages), and return where it
/// is.
async function parkTouch(page: Page, row: Locator): Promise<number> {
  await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
  await settle(page);
  const top = (await scrollMetrics(page)).top;
  expect(top, "premise: room for the whole drag").toBeGreaterThan(DRAG_PX + 20);
  return top;
}

/// Where a drag starts: in the history, its whole path clear of the picker
/// (if open) and the composer.
async function dragStart(page: Page): Promise<[number, number]> {
  const view = (await page.locator("#chat-scroll-container").boundingBox())!;
  const p = (await isOpen(page)) ? await picker(page).boundingBox() : null;
  const lowerHalf = !p || p.y > view.y + view.height / 2;
  const [x, y] = [Math.round(view.x + 30), Math.round(lowerHalf ? view.y + 40 : view.y + view.height - 40 - DRAG_PX)];
  for (const at of [y, y + DRAG_PX]) expect(await hitAt(page, x, at), `premise: (${x}, ${at}) is on the history`).toBe("history");
  return [x, y];
}

/// With the history parked at `row` and its picker open, one finger down on
/// the history and moved past a tap's slop, not lifted: the picker captures
/// it, stays open, and the history holds. Returns where the drag started.
async function startHeldDrag(page: Page, fingers: Fingers, row: Locator): Promise<[number, number]> {
  const top = await parkTouch(page, row);
  await openFromKeyboard(page, row);
  const [x, y] = await dragStart(page);
  await fingers.press([1, x, y]);
  await fingers.move({ 1: DRAG_PX / 2 });
  expect(await pickerCaptures(page), "premise: past a tap's slop, the picker captured the finger").toHaveLength(1);
  expect(await isOpen(page), "a drag does not close it").toBe(true);
  expect((await scrollMetrics(page)).top, "held").toBe(top);
  return [x, y];
}

/// A one-finger drag at the drag start, settled; how far the history moved.
async function dragMoves(page: Page, fingers: Fingers): Promise<number> {
  const [x, y] = await dragStart(page);
  const before = (await scrollMetrics(page)).top;
  await fingers.drag(x, y);
  await settle(page);
  return (await scrollMetrics(page)).top - before;
}

test.describe("A finger on the history and the reaction picker", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "real touch input needs Chromium's input protocol");

  test("a one-finger drag scrolls the history closed, not open, and again once it closes", async ({ page }) => {
    const row = await touchRoom(page, "drag filler");
    await withFingers(page, async (fingers) => {
      await parkTouch(page, row);
      expect(await dragMoves(page, fingers), "premise: closed, the drag scrolls the history").toBeLessThan(-20);

      await parkTouch(page, row);
      await openFromKeyboard(page, row);
      expect(await dragMoves(page, fingers), "open: the drag scrolled nothing").toBe(0);
      expect(await isOpen(page), "a completed drag is not a tap: still open").toBe(true);
      expect(await pickerCaptures(page), "the drag's end let go of its capture").toEqual([]);

      await page.keyboard.press("Escape");
      await expectClosed(page);
      await parkTouch(page, row);
      expect(await dragMoves(page, fingers), "closed again: the drag scrolls").toBeLessThan(-20);
    });
  });

  test("once arrivals carry its React out of the history's view, the next drag closes it and scrolls", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "carried touch anchor");
    await recordTouch(page);
    const row = committedRow(page, "carried touch anchor");
    await withFingers(page, async (fingers) => {
      await openFromKeyboard(page, row);
      for (let i = 0; i < 30 && !(await reactOutOfView(page, row)); i++) {
        await deliver(page, `carried touch filler ${i}: ${"t".repeat(200)}`);
      }
      expect(await reactOutOfView(page, row), "premise: arrivals carried React out of the history's view").toBe(true);
      expect(await isOpen(page), "premise: arrivals alone do not close it").toBe(true);
      expect(await dragMoves(page, fingers), "the drag scrolled the history").toBeLessThan(-20);
      await expectClosed(page);
      expect(await pickerCaptures(page), "nothing left captured").toEqual([]);
    });
  });

  test("a drag under way when it closes keeps no capture, and the next drag scrolls", async ({ page }) => {
    const row = await touchRoom(page, "midway filler");
    await withFingers(page, async (fingers) => {
      await startHeldDrag(page, fingers, row);

      await page.keyboard.press("Escape");
      await expectClosed(page);
      expect(await pickerCaptures(page), "closing let go of the finger it captured").toEqual([]);
      await fingers.lift();
      await settle(page);
      await expectClosed(page);

      await parkTouch(page, row);
      expect(await dragMoves(page, fingers), "a fresh drag scrolls").toBeLessThan(-20);
    });
  });

  test("a cancelled drag keeps nothing: the next one is still held, and once it closes one scrolls", async ({ page }) => {
    const row = await touchRoom(page, "cancel filler");
    await withFingers(page, async (fingers) => {
      await startHeldDrag(page, fingers, row);
      const top = (await scrollMetrics(page)).top;
      await fingers.cancel();
      await settle(page);
      expect(await pickerCaptures(page), "the cancel let go of it").toEqual([]);
      expect(await isOpen(page), "a cancel does not close it").toBe(true);
      expect((await scrollMetrics(page)).top).toBe(top);

      expect(await dragMoves(page, fingers), "open: the next drag is held").toBe(0);
      expect(await isOpen(page)).toBe(true);

      await page.keyboard.press("Escape");
      await expectClosed(page);
      await parkTouch(page, row);
      expect(await dragMoves(page, fingers), "closed: a drag scrolls").toBeLessThan(-20);
    });
  });

  test("a second finger joining a drag is neither held nor captured", async ({ page }) => {
    const row = await touchRoom(page, "second filler");
    await withFingers(page, async (fingers) => {
      await takeTouchMoves(page);
      const [x, y] = await startHeldDrag(page, fingers, row);
      const single = await takeTouchMoves(page);
      expect(single.length, "premise: the first finger moved").toBeGreaterThan(0);
      expect(single.every((m) => m.touches === 1 && m.prevented), "one finger: held").toBe(true);

      await fingers.press([2, x + 120, y]);
      expect(await pickerCaptures(page), "a second finger: the first is let go").toEqual([]);
      await fingers.move({ 1: -DRAG_PX / 2, 2: DRAG_PX / 2 });
      const pinch = await takeTouchMoves(page);
      expect(pinch.length, "premise: both fingers moved").toBeGreaterThan(0);
      expect(pinch.filter((m) => m.touches === 2 && !m.prevented).length, "two fingers: let through").toBe(pinch.length);
      expect(await pickerCaptures(page), "neither finger captured").toEqual([]);
      await fingers.lift();
      await settle(page);
      // Whether the browser zooms on a pinch whose first touchmoves were held
      // is its own business; a pinch from the start is checked below.
      const scale = await page.evaluate(() => window.visualViewport!.scale);
      test.info().annotations.push({ type: "after the joined pinch", description: `scale ${scale}, open ${await isOpen(page)}` });
      if (scale !== 1) await fingers.cdp.send("Emulation.setPageScaleFactor", { pageScaleFactor: 1 });

      if (await isOpen(page)) await page.keyboard.press("Escape");
      await expectClosed(page);
      await parkTouch(page, row);
      expect(await dragMoves(page, fingers), "closed: a drag scrolls").toBeLessThan(-20);
    });
  });

  test("a pinch zooms while it is open", async ({ page }) => {
    const row = await touchRoom(page, "pinch filler");
    await withFingers(page, async (fingers) => {
      const scale = () => page.evaluate(() => window.visualViewport!.scale);
      const pinchAt = async ([x, y]: [number, number]) => {
        await fingers.press([1, x, y], [2, x, y + 40]);
        await fingers.move({ 1: -60, 2: 60 }, 12);
        await fingers.lift();
        await settle(page);
      };
      await parkTouch(page, row);
      await pinchAt(await dragStart(page));
      expect(await scale(), "premise: closed, the pinch zooms").toBeGreaterThan(1.2);
      await fingers.cdp.send("Emulation.setPageScaleFactor", { pageScaleFactor: 1 });
      await expect.poll(scale, { message: "premise: zoom reset" }).toBeCloseTo(1, 1);

      await parkTouch(page, row);
      await openFromKeyboard(page, row);
      await pinchAt(await dragStart(page));
      expect(await scale(), "open: the pinch zooms").toBeGreaterThan(1.2);
    });
  });

  // Simulated: the hold's listeners fail to come off when it closes. Left
  // attached, they hold nothing while the picker is closed.
  test("a hold whose listeners fail to come off holds nothing once it closes", async ({ page }) => {
    const row = await touchRoom(page, "detach filler");
    const kinds = ["wheel", "touchmove", "pointerdown", "pointermove", "pointerup", "pointercancel", "lostpointercapture"];
    try {
      await withFingers(page, async (fingers) => {
        await parkTouch(page, row);
        await openFromKeyboard(page, row);
        await page.evaluate((kinds) => {
          const w = window as unknown as { __kept: number };
          w.__kept = 0;
          const remove = EventTarget.prototype.removeEventListener;
          window.removeEventListener = function (this: Window, type: string, ...rest: unknown[]) {
            if (kinds.includes(type)) {
              w.__kept++;
              return;
            }
            return (remove as (...a: unknown[]) => void).call(this, type, ...rest);
          } as typeof window.removeEventListener;
        }, kinds);
        await page.keyboard.press("Escape");
        await expectClosed(page);
        await settle(page);
        expect(await page.evaluate(() => (window as unknown as { __kept: number }).__kept), "premise: the hold's removals were refused").toBeGreaterThan(0);

        await parkTouch(page, row);
        expect(await dragMoves(page, fingers), "closed, listeners still on: the drag scrolls").toBeLessThan(-20);
        if (!test.info().project.name.startsWith("mobile")) {
          const [x, y] = await dragStart(page);
          expect(await wheelCancelled(page, x, y, false), "closed, listeners still on: a wheel is let through").toBe(false);
        }
      });
    } finally {
      await page.evaluate(() => delete (window as unknown as { removeEventListener?: unknown }).removeEventListener).catch(() => {});
    }
  });
});

// The picker is a native popover or nothing (reaction_picker.rs). Simulated:
// every engine this suite runs has the Popover API, so it is taken away, or
// made to throw, in the page.

const UNAVAILABLE = "Update your browser to add reactions.";
const OPEN_FAILED = "Couldn’t open reactions. Try again.";

/// Before the app loads: no `showPopover`, as in a browser without popovers.
async function withoutPopoverApi(page: Page) {
  await page.addInitScript(() => {
    Object.defineProperty(HTMLElement.prototype, "showPopover", { value: undefined, configurable: true, writable: true });
  });
}

/// From now on every `showPopover` call is counted, and throws while
/// `__showThrows` is "always", or only when given options while it is
/// "options". `restoreShowPopover` puts the browser's own back.
function patchShowPopover(page: Page, mode: "always" | "options") {
  return page.evaluate((mode) => {
    const w = window as unknown as { __realShow?: (this: HTMLElement, ...a: unknown[]) => void; __showAttempts: number; __showThrows: string };
    w.__realShow ??= HTMLElement.prototype.showPopover;
    w.__showAttempts ??= 0;
    w.__showThrows = mode;
    HTMLElement.prototype.showPopover = function (this: HTMLElement, ...args: unknown[]) {
      w.__showAttempts++;
      if (w.__showThrows === "always" || args.length > 0) throw new DOMException("simulated", "NotSupportedError");
      return w.__realShow!.call(this);
    } as typeof HTMLElement.prototype.showPopover;
  }, mode);
}

function restoreShowPopover(page: Page) {
  return page.evaluate(() => {
    HTMLElement.prototype.showPopover = (window as unknown as { __realShow: typeof HTMLElement.prototype.showPopover }).__realShow;
  });
}

function showAttempts(page: Page): Promise<number> {
  return page.evaluate(() => (window as unknown as { __showAttempts?: number }).__showAttempts ?? 0);
}

/// Record every toast card that mounts from now on. The card is keyed by its
/// toast's id, so each `show_toast` mounts one, even with the same message.
async function countToasts(page: Page) {
  await page.evaluate(() => {
    const w = window as unknown as { __toasts: Element[] };
    w.__toasts = [];
    new MutationObserver((records) => {
      for (const r of records)
        for (const n of Array.from(r.addedNodes)) {
          if (!(n instanceof Element)) continue;
          const card = n.matches('[data-testid="toast"]') ? n : n.querySelector('[data-testid="toast"]');
          if (card) w.__toasts.push(card);
        }
    }).observe(document.querySelector('[data-testid="toast-host"]')!, { childList: true, subtree: true });
  });
}

function toastsSeen(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    (window as unknown as { __toasts: Element[] }).__toasts.map((t) => t.querySelector('[data-testid="toast-message"]')?.textContent?.trim() ?? ""),
  );
}

/// How many Reacts carry the picker's anchor name.
function anchoredReacts(page: Page): Promise<number> {
  return page.evaluate(
    () => Array.from(document.querySelectorAll<HTMLElement>('[data-testid="message-react-button"]')).filter((b) => b.style.getPropertyValue("anchor-name") !== "").length,
  );
}

/// Nothing an opening commits is left: the picker closed and hidden, no React
/// expanded or anchoring it, no input held, nothing making the app busy, and
/// nothing signed.
async function expectNothingOpened(page: Page, row: Locator, signs: number) {
  expect(await isOpen(page), "the picker is not open").toBe(false);
  await expect(picker(page)).toBeHidden();
  expect(await expandedRows(page), "no React is expanded").toEqual([]);
  expect(await anchoredReacts(page), "no React anchors the picker").toBe(0);
  const view = (await page.locator("#chat-scroll-container").boundingBox())!;
  const [x, y] = [view.x + 30, view.y + 30];
  expect(await hitAt(page, x, y), "premise: the history point hits the history").toBe("history");
  expect(await wheelCancelled(page, x, y, false), "no input is held").toBe(false);
  expect(await callRiverTest(page, "userIsBusy"), "not busy").toBe(false);
  expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signs);
  await expect(row.getByTestId("reaction-chip"), "no reaction").toHaveCount(0);
}

test.describe("Without a working Popover API", () => {
  // A further simulation, not a run in an old browser: such a browser also
  // has no built-in rule hiding a closed `[popover]`, so that attribute is
  // taken off the picker first. Only the stylesheet then keeps the emoji out
  // of the page, for everything below.
  test("a browser without it keeps the picker out of the page, gets a disabled React that says why, from the keyboard too, and nothing opens", async ({ page, browserName }) => {
    // WebKit's Tab reaches only text fields; Alt+Tab reaches every control.
    const tab = browserName === "webkit" ? "Alt+Tab" : "Tab";
    await withoutPopoverApi(page);
    await openRoomWithComposer(page);
    await deliver(page, "unsupported target");
    const row = committedRow(page, "unsupported target");
    const react = row.getByTestId("message-react-button");
    const tip = row.getByTestId("message-react-unavailable");
    await page.getByTestId("message-input").blur();
    await settle(page);
    const geometry = () =>
      page.evaluate(() => {
        const box = (sel: string) => {
          const r = document.querySelector(sel)!.getBoundingClientRect();
          return [r.left, r.top, r.width, r.height].map((v) => Math.round(v));
        };
        return { history: box("#chat-scroll-container"), composer: box('[data-testid="message-composer"]') };
      });
    const before = await geometry();
    await picker(page).evaluate((el) => el.removeAttribute("popover"));
    await nextFrames(page);
    expect(await picker(page).getAttribute("popover"), "premise: no popover attribute").toBeNull();
    expect(await picker(page).evaluate((el) => getComputedStyle(el).display), "hidden by the stylesheet").toBe("none");
    expect(await geometry(), "the history and composer did not move").toEqual(before);
    const signs = await callRiverTest(page, "messageSignsRecorded");

    await expect(react).toHaveAttribute("aria-disabled", "true");
    await expect(react).toHaveAccessibleName("React");
    await expect(react, "the explanation is React's description").toHaveAccessibleDescription(UNAVAILABLE);
    await expect(react, "nothing to expand").not.toHaveAttribute("aria-expanded");
    await expect(react, "no popup to announce").not.toHaveAttribute("aria-haspopup");
    await expect(picker(page), "the picker is still mounted, closed").toHaveCount(1);
    await expect(picker(page)).toBeHidden();
    await expect(tip, "the explanation is not painted at rest").toBeHidden();

    // From the keyboard: Tab from the control before it lands on it, which
    // shows it and its explanation; Enter and Space open nothing.
    await page.mouse.move(0, 0);
    await react.evaluate((el) => {
      const focusable = Array.from(document.querySelectorAll<HTMLElement>("button, a[href], input, textarea, select, [tabindex]")).filter(
        (e) => e.tabIndex >= 0 && !(e as HTMLButtonElement).disabled && e.getClientRects().length > 0,
      );
      focusable[focusable.indexOf(el as HTMLElement) - 1].focus();
    });
    await page.keyboard.press(tab);
    await expect(react, "Tab reaches it").toBeFocused();
    await expect(tip, "keyboard focus shows why").toBeVisible();
    await expect(tip).toHaveText(UNAVAILABLE);
    await expect.poll(() => react.evaluate((el) => getComputedStyle(el).opacity), { message: "focused, it shows" }).toBe("1");
    await page.keyboard.press("Enter");
    await page.keyboard.press("Space");
    await settle(page);
    await expect(react, "focus stays on it").toBeFocused();
    await expectNothingOpened(page, row, signs);

    // A pointer press opens nothing either. Pressed by position, not by an
    // automation click, which would wait for it to be enabled.
    await react.blur();
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await settle(page);
    await revealFooter(page, row);
    const { x, y, lands } = await react.evaluate((el) => {
      const r = el.getBoundingClientRect();
      const [x, y] = [r.left + r.width / 2, r.top + r.height / 2];
      return { x, y, lands: el.contains(document.elementFromPoint(x, y)) };
    });
    expect(lands, "premise: the press lands on React").toBe(true);
    if (await coarsePointer(page)) await page.touchscreen.tap(x, y);
    else {
      await page.mouse.click(x, y);
      await expect(tip, "hovered, it says why").toBeVisible();
    }
    await settle(page);
    await react.blur();
    await expectNothingOpened(page, row, signs);
  });

  test("a popover that throws is undone, says to try again once per press, and a retry opens it", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "throwing target");
    const row = committedRow(page, "throwing target");
    const react = row.getByTestId("message-react-button");
    await page.getByTestId("message-input").blur();
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await countToasts(page);
    await patchShowPopover(page, "always");
    await expect(react, "a browser with popovers: React is enabled").not.toHaveAttribute("aria-disabled");

    await page.mouse.move(0, 0);
    await react.focus();
    await page.keyboard.press("Enter");
    await expect.poll(() => toastsSeen(page), { message: "one message for the press" }).toEqual([OPEN_FAILED]);
    expect(await showAttempts(page), "premise: both ways of showing it were tried").toBe(2);
    await settle(page);
    await expectNothingOpened(page, row, signs);
    await expect(react, "focus stays on React, for a retry").toBeFocused();
    await expect(react, "and React stays enabled").not.toHaveAttribute("aria-disabled");
    await expect(page.getByTestId("toast-message")).toHaveText(OPEN_FAILED);

    // A second press: one more message, not one per attempt.
    await page.keyboard.press("Enter");
    await expect.poll(() => showAttempts(page)).toBe(4);
    await settle(page);
    expect(await toastsSeen(page), "one message per press").toEqual([OPEN_FAILED, OPEN_FAILED]);
    await expectNothingOpened(page, row, signs);

    // An engine that only rejects the `source` option: the plain call opens
    // it, with no message.
    await patchShowPopover(page, "options");
    await page.keyboard.press("Enter");
    await expectOpenFor(page, row);
    expect(await toastsSeen(page), "the fallback opening says nothing").toHaveLength(2);
    await page.keyboard.press("Escape");
    await expectClosed(page);

    // Restored: a retry from where focus was left opens it, and a pick acts.
    await restoreShowPopover(page);
    await expect(react).toBeFocused();
    await page.keyboard.press("Enter");
    await expectOpenFor(page, row);
    await picker(page).getByRole("button").first().focus();
    await page.keyboard.press("Enter");
    await expectClosed(page);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);
    expect(await callRiverTest(page, "messageSignsRecorded"), "one pick, signed once").toBe(signs + 1);
    expect(await toastsSeen(page), "no message after the retry").toHaveLength(2);
  });

  test("a failed re-showing after a remount closes quietly, and a later opening works", async ({ page }) => {
    await openRoomWithComposer(page);
    for (let i = 0; i < 4; i++) await deliver(page, `quiet filler ${i}: ${"q".repeat(200)}`);
    // The newest group, as in the remount test above.
    await callRiverTest(page, "deliverTimedMessages", 0, [
      { text: "quiet lead", sentSecs: 1, receivedSecs: 0 },
      { text: "quiet survivor", sentSecs: 2, receivedSecs: 0 },
    ]);
    const [lead, survivor] = [committedRow(page, "quiet lead"), committedRow(page, "quiet survivor")];
    await expect(survivor).toHaveCount(1);
    const groupKey = (row: Locator) => row.evaluate((el) => el.closest(".msg-group")?.getAttribute("data-item-key") ?? null);
    const leadKey = await rowKey(lead);
    expect(await groupKey(survivor), "premise: one group, keyed by the lead").toBe(leadKey);
    await page.getByTestId("message-input").blur();
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await countToasts(page);

    // Remounted with the picker open, and showing it at the new React throws.
    await openFromKeyboard(page, survivor);
    await patchShowPopover(page, "always");
    await callRiverTest(page, "removeMessages", "quiet lead");
    await expect(lead).toHaveCount(0);
    await expect.poll(() => groupKey(survivor), { message: "premise: the group re-keyed" }).not.toBe(leadKey);
    await expect.poll(() => showAttempts(page), { message: "premise: it tried to show it at the new React" }).toBeGreaterThan(0);
    await settle(page);
    await expectClosed(page);
    await expectNothingOpened(page, survivor, signs);
    expect(await toastsSeen(page), "the reader pressed nothing: no message").toEqual([]);

    await restoreShowPopover(page);
    await openFromKeyboard(page, survivor);
    await page.keyboard.press("Escape");
    await expectClosed(page);
  });

  // Dispatch order only, so run once: a press whose message is gone by the
  // time it runs, and the first of two presses in one task.
  test("with a popover that throws, a stale press tries nothing and a superseded one gives way to the later press", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "dispatch target");
    const row = committedRow(page, "dispatch target");
    await page.getByTestId("message-input").blur();
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await countToasts(page);
    await patchShowPopover(page, "always");

    // A press whose message is gone by the time it runs (a room switch in
    // the same task) tries nothing and says nothing.
    const attempts = await showAttempts(page);
    const id = (await row.getAttribute("id"))!;
    await page.evaluate((id) => {
      const hooks = (window as unknown as { __riverTest: { switchRoom(n: string): void } }).__riverTest;
      hooks.switchRoom("Team Chat Room");
      document.querySelector<HTMLElement>(`[id="${id}"] [data-testid="message-react-button"]`)!.click();
    }, id);
    await expect(page.locator(`[id="${id}"]`)).toHaveCount(0);
    await settle(page);
    expect(await showAttempts(page), "a stale press tries nothing").toBe(attempts);
    expect(await toastsSeen(page), "and says nothing").toEqual([]);
    await callRiverTest(page, "switchRoom", "Your Private Room");
    await expect(row).toHaveCount(1);

    // Two presses in one task: the first is superseded before it runs, so
    // only the second tries, and says so once.
    await page.evaluate((id) => {
      const react = document.querySelector<HTMLElement>(`[id="${id}"] [data-testid="message-react-button"]`)!;
      react.click();
      react.click();
    }, id);
    await expect.poll(() => toastsSeen(page)).toEqual([OPEN_FAILED]);
    await settle(page);
    expect(await showAttempts(page), "only the later press tried").toBe(attempts + 2);
    expect(await toastsSeen(page)).toEqual([OPEN_FAILED]);
    await expectNothingOpened(page, row, signs);

    await restoreShowPopover(page);
    await openFromKeyboard(page, row);
    await page.keyboard.press("Escape");
    await expectClosed(page);
  });
});
