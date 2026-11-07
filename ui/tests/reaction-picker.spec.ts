import { test, expect, Locator, Page } from "@playwright/test";
import { coarsePointer, nextFrames, openRoomWithComposer, revealFooter, settle } from "./example-room";
import { deliver, deliverOffscreen, expectFollowed, readerScrollsWithoutGesture, scrollMetrics } from "./history-geometry";
import { callRiverTest } from "./river-test";

// The conversation's one reaction picker: a native `popover="auto"`, opened
// at a message's footer React and anchored to it (reaction_picker.rs,
// `.reaction-picker` in main.css). The browser closes it on Escape and on a
// press outside it, which still reaches what was pressed; a pick, a room
// switch and its message leaving the history close it too. While it is open
// the reader's wheel and touch scrolling are held off, as the full-screen
// backdrop it replaced did, but arrivals are still followed.

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

function committed(page: Page, text: string): Locator {
  return page.getByTestId("conversation-history").getByTestId("message-row").filter({ hasText: text });
}

async function send(page: Page, text: string) {
  const input = page.getByTestId("message-input");
  await input.fill(text);
  await input.press("Enter");
  await expect(committed(page, text)).toHaveCount(1);
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

type Box = { left: number; right: number; top: number; bottom: number };

/// The open picker, the React it hangs from, the viewport, and whether each
/// emoji is the element a press at its centre lands on.
function measure(page: Page, row: Locator) {
  return row.evaluate((el, sel) => {
    const box = (e: Element): Box => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    const p = document.querySelector(sel)!;
    const react = el.querySelector('[data-testid="message-react-button"]')!;
    const buttons = Array.from(p.querySelectorAll("button")).map((b) => {
      const r = b.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {
        emoji: (b.textContent || "").trim(),
        inside: r.left >= -0.5 && r.top >= -0.5 && r.right <= innerWidth + 0.5 && r.bottom <= innerHeight + 0.5,
        hit: !!at && (at === b || b.contains(at)),
      };
    });
    return { picker: box(p), react: box(react), vw: innerWidth, vh: innerHeight, buttons };
  }, PICKER);
}
type Measured = Awaited<ReturnType<typeof measure>>;

const GAP = 4;

/// Where the picker sits against its React: below or above it (4px apart),
/// and its right edge on React's right edge or its left edge on React's left.
function placement(m: Measured) {
  const { picker: p, react: r } = m;
  const near = (a: number, b: number) => Math.abs(a - b) <= 1;
  return {
    block: near(p.top, r.bottom + GAP) ? "below" : near(p.bottom, r.top - GAP) ? "above" : `off (${p.top}..${p.bottom} vs ${r.top}..${r.bottom})`,
    inline: near(p.right, r.right) ? "end" : near(p.left, r.left) ? "start" : `off (${p.left}..${p.right} vs ${r.left}..${r.right})`,
  };
}

function expectEveryEmojiUsable(m: Measured) {
  expect(m.buttons.length, "the picker has its emoji").toBe(16);
  const outside = m.buttons.find((b) => !b.inside);
  expect(outside, `${outside?.emoji} is outside the viewport`).toBeUndefined();
  const covered = m.buttons.find((b) => !b.hit);
  expect(covered, `${covered?.emoji} is not what a press at its centre lands on`).toBeUndefined();
}

/// The picker never covers the React it hangs from.
function expectInvokerUncovered(m: Measured) {
  const { picker: p, react: r } = m;
  const overlaps = p.left < r.right && p.right > r.left && p.top < r.bottom && p.bottom > r.top;
  expect(overlaps, "the picker covers its React").toBe(false);
}

test.describe("The reaction picker", () => {
  test("is one native popover: Escape and a press outside close it, and the press still lands", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "native target");
    const row = committed(page, "native target");

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

    // A press outside closes it, and reaches what it pressed: the row's own
    // Reply, beside the picker, starts a reply on that same press.
    await openWithPointer(page, row);
    const reply = row.getByTestId("message-reply-button");
    const box = (await reply.boundingBox())!;
    const hit = await page.evaluate(([x, y]) => document.elementFromPoint(x, y)?.closest("button")?.getAttribute("data-testid"), [box.x + box.width / 2, box.y + box.height / 2]);
    expect(hit, "premise: Reply is uncovered").toBe("message-reply-button");
    if (await coarsePointer(page)) await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);
    else await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expectClosed(page);
    await expect(page.getByTestId("message-composer")).toContainText("Replying to");

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
    const row = committed(page, "pick target");
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
  test("a pick on a message that already has reactions keeps the others' reactions", async ({ page }) => {
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
    const row = committed(page, "render target");
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
    const [one, two] = [committed(page, "rapid one"), committed(page, "rapid two")];

    // A press on another React while it is open.
    await openWithPointer(page, one);
    await openWithPointer(page, two);
    expect(placement(await measure(page, two)).block).not.toContain("off");

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
    const row = committed(page, "switch target");
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
    await send(page, "doomed target");
    const row = committed(page, "doomed target");
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

  test("a reaction arriving while it is open moves React, and the picker with it", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliverOffscreen(page, "reflow target");
    const row = committed(page, "reflow target");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await openFromKeyboard(page, row);
    const before = await measure(page, row);
    await callRiverTest(page, "addReactionsToNewest", ["🎉", "👀", "🔥"]);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(3);
    await nextFrames(page);
    const after = await measure(page, row);
    expect(after.react.left - before.react.left, "premise: the chips moved React").toBeGreaterThan(20);
    expect(placement(after)).toEqual(placement(before));
    await expectOpenFor(page, row);
  });
});

test.describe("Reaction picker placement", () => {
  test.describe("at 320px", () => {
    test.use({ viewport: { width: 320, height: 700 } });

    test("at a React with no chips it opens toward the free side, and at one at the column's end, back from it", async ({ page }) => {
      await openRoomWithComposer(page);
      await deliverOffscreen(page, "start edge");
      // Room below it, so only the inline side has to give.
      for (let i = 0; i < 3; i++) await deliverOffscreen(page, `start filler ${i}: ${"s".repeat(200)}`);
      const start = committed(page, "start edge");
      await start.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await openFromKeyboard(page, start);
      let m = await measure(page, start);
      expect(m.react.right - m.react.left, "premise").toBeGreaterThan(0);
      expect(m.react.right - (m.picker.right - m.picker.left), "premise: no room back from React").toBeLessThan(0);
      expect(placement(m)).toEqual({ block: "below", inline: "start" });
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
      await page.keyboard.press("Escape");

      // Chips put the newest message's React well into the column: it opens
      // back from React, its right edge on React's.
      const end = committed(page, "start filler 2");
      await callRiverTest(page, "addReactionsToNewest", ["👍", "❤️", "😂"]);
      await expect(end.getByTestId("reaction-chip")).toHaveCount(3);
      await openFromKeyboard(page, end);
      m = await measure(page, end);
      expect(m.react.right - (m.picker.right - m.picker.left), "premise: room back from React").toBeGreaterThan(0);
      expect(placement(m).inline).toBe("end");
      expect(["below", "above"]).toContain(placement(m).block);
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
    });
  });

  test.describe("in a short landscape view", () => {
    test.use({ viewport: { width: 740, height: 360 } });

    test("at the newest message's React it opens above, and toward the free side", async ({ page }) => {
      await openRoomWithComposer(page);
      await deliver(page, "corner target");
      const row = committed(page, "corner target");
      await openFromKeyboard(page, row);
      const m = await measure(page, row);
      expect(m.vh - m.react.bottom, "premise: no room below React").toBeLessThan(m.picker.bottom - m.picker.top + GAP);
      expect(m.react.right - (m.picker.right - m.picker.left), "premise: no room back from React").toBeLessThan(0);
      expect(placement(m)).toEqual({ block: "above", inline: "start" });
      expectEveryEmojiUsable(m);
      expectInvokerUncovered(m);
    });
  });

  // Simulated: every engine this suite runs supports anchor positioning, so
  // the rule is taken out of the stylesheet.
  test("without anchor positioning it opens centred in the viewport, and still works", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "fallback target");
    const row = committed(page, "fallback target");
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
    const m = await measure(page, row);
    expect(Math.abs(m.picker.left - (m.vw - m.picker.right)), "centred across").toBeLessThanOrEqual(1);
    expect(Math.abs(m.picker.top - (m.vh - m.picker.bottom)), "centred down").toBeLessThanOrEqual(1);
    expectEveryEmojiUsable(m);
    await picker(page).getByRole("button").first().click();
    await expectClosed(page);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);
  });
});

test.describe("While the reaction picker is open", () => {
  test("the reader's wheel does not scroll the history; closing it lets it again", async ({ page }) => {
    test.skip(test.info().project.name.startsWith("mobile"), "no wheel on a phone");
    await openRoomWithComposer(page);
    for (let i = 0; i < 6; i++) await deliver(page, `wheel filler ${i}: ${"w".repeat(200)}`);
    const row = committed(page, "wheel filler 3");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await settle(page);
    await openWithPointer(page, row);
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const p = (await picker(page).boundingBox())!;
    const y = p.y > view.y + view.height / 2 ? view.y + 30 : view.y + view.height - 30;

    for (const [where, x, at] of [
      ["over the history", view.x + 30, y],
      ["over the picker", p.x + p.width / 2, p.y + p.height / 2],
    ] as const) {
      const before = (await scrollMetrics(page)).top;
      await page.mouse.move(x, at);
      await page.mouse.wheel(0, -200);
      await settle(page);
      expect((await scrollMetrics(page)).top, `${where}: the history did not move`).toBe(before);
      expect(await isOpen(page), `${where}: still open`).toBe(true);
    }

    await page.keyboard.press("Escape");
    await expectClosed(page);
    const before = (await scrollMetrics(page)).top;
    await page.mouse.move(view.x + 30, y);
    await page.mouse.wheel(0, -200);
    await settle(page);
    expect((await scrollMetrics(page)).top, "closed: the wheel scrolls").toBeLessThan(before);
  });

  test("a touch drag does not scroll the history; closing it lets it again", async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "a real touch drag needs Chromium's input protocol");
    await openRoomWithComposer(page);
    for (let i = 0; i < 6; i++) await deliver(page, `drag filler ${i}: ${"d".repeat(200)}`);
    const row = committed(page, "drag filler 3");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await settle(page);
    await openFromKeyboard(page, row);
    const view = (await page.locator("#chat-scroll-container").boundingBox())!;
    const p = (await picker(page).boundingBox())!;
    const y = Math.round(p.y > view.y + view.height / 2 ? view.y + 40 : view.y + view.height - 120);
    const cdp = await page.context().newCDPSession(page);
    const drag = () =>
      cdp.send("Input.synthesizeScrollGesture", { x: Math.round(view.x + 30), y, yDistance: 80, gestureSourceType: "touch", speed: 800 });

    const before = (await scrollMetrics(page)).top;
    await drag();
    await settle(page);
    expect((await scrollMetrics(page)).top, "open: the drag scrolled nothing").toBe(before);
    expect(await isOpen(page)).toBe(true);

    await page.keyboard.press("Escape");
    await expectClosed(page);
    await drag();
    await settle(page);
    expect((await scrollMetrics(page)).top, "closed: the drag scrolls").toBeLessThan(before);
  });

  test("an arrival is still followed", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "follow anchor");
    const row = committed(page, "follow anchor");
    await openFromKeyboard(page, row);
    await deliver(page, "followed while open");
    await expectFollowed(page, "followed while open", "an arrival follows under the open picker");
    await expectOpenFor(page, row);
  });

  test("an automatic reload waits, and closing it does not release one an unsent message holds", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "reload target");
    const row = committed(page, "reload target");
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
    const input = page.getByTestId("message-input");
    await input.fill("held while picking");
    await input.press("Enter");
    await expect(input).toHaveValue("");
    await openFromKeyboard(page, row);
    await page.keyboard.press("Escape");
    await expectClosed(page);
    await row.getByTestId("message-react-button").blur();
    expect(await callRiverTest(page, "userIsBusy"), "the unsent message still holds it").toBe(true);
    await callRiverTest(page, "releaseMessageSigning");
    await expect(committed(page, "held while picking")).toHaveCount(1);
    await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "signed: released" }).toBe(false);
  });

  // The app toast and the picker can meet: the picker paints over the toast
  // where they overlap, and a press on an uncovered toast button closes the
  // picker and runs the button on that same press.
  test("an uncovered toast action runs once on the press that closes the picker; a covered one waits", async ({ page }) => {
    await page.setViewportSize({ width: 320, height: 700 });
    await openRoomWithComposer(page);
    await deliver(page, "toast target");
    const row = committed(page, "toast target");
    await callRiverTest(page, "takeToastActionRuns");

    await openFromKeyboard(page, row);
    await callRiverTest(page, "showToast", "Thread archived", "Undo");
    const action = page.getByTestId("toast-action");
    await expect(action).toBeVisible();
    const overlap = await page.evaluate((sel) => {
      const a = document.querySelector(sel)!.getBoundingClientRect();
      const b = document.querySelector('[data-testid="toast"]')!.getBoundingClientRect();
      return Math.max(0, Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top)) * Math.max(0, Math.min(a.right, b.right) - Math.max(a.left, b.left));
    }, PICKER);
    test.info().annotations.push({ type: "320px overlap", description: `${overlap}px²` });
    const box = (await action.boundingBox())!;
    const hit = await page.evaluate(([x, y]) => document.elementFromPoint(x, y)?.getAttribute("data-testid") ?? null, [box.x + box.width / 2, box.y + box.height / 2]);
    expect(hit, "premise: the action is uncovered").toBe("toast-action");
    if (await coarsePointer(page)) await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);
    else await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expectClosed(page);
    expect(await callRiverTest(page, "takeToastActionRuns"), "the action ran once, on that press").toBe(1);
    await expect(action).toHaveCount(0);
    const focused = await page.evaluate(() => document.activeElement?.getAttribute("data-testid") ?? document.activeElement?.tagName);
    test.info().annotations.push({ type: "focus after the action", description: String(focused) });

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
