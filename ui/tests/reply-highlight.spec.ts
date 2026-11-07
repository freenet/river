import { test, expect, Locator, Page } from "@playwright/test";
import { coarsePointer, openRoomWithComposer, resolveColor, revealFooter, settle } from "./example-room";
import {
  deliverOffscreen,
  distanceFromBottom,
  expectFollowed,
  expectParkedAwayFromEnd,
  expectSettledAtBottom,
  readingRow,
  READING_ROW_BUDGET_PX,
  scrollMetrics,
} from "./history-geometry";
import { callRiverTest } from "./river-test";

// A reply's quote strip is a native button that jumps the room history to the
// quoted message and highlights that message's whole row for two seconds
// (`conversation/reply_highlight.rs`). Policy for where the view goes after:
// `.claude/rules/history-scrolling.md`.
//
// The highlight is a Web Animations API animation with the id
// `reply-highlight`; the tests drive it by pausing and seeking it, never by
// sampling wall-clock frames.

const HIGHLIGHT = "reply-highlight";
/// The clearance above a target taller than the view.
const TALL_CLEARANCE_PX = 12;

function committed(page: Page, text: string): Locator {
  return page.getByTestId("conversation-history").getByTestId("message-row").filter({ hasText: text });
}

async function send(page: Page, text: string) {
  const input = page.getByTestId("message-input");
  await input.fill(text);
  await input.press("Enter");
  await expect(committed(page, text)).toHaveCount(1);
}

/// The row a strip quotes, found by the strip's own `data-reply-target`.
function targetOf(page: Page, strip: Locator): Promise<Locator> {
  return strip.getAttribute("data-reply-target").then((id) => {
    expect(id, "premise: the strip names its target").toMatch(/^msg-/);
    return page.locator(`[id="${id}"]`);
  });
}

/// An own message `target` (sent as `body`, by default the same text), then
/// enough history below it that it is well above the view, then an own reply
/// quoting it at the end. Returns the reply's strip and the quoted row.
async function replyFarBelow(page: Page, target: string, reply: string, body = target) {
  await openRoomWithComposer(page);
  await page.mouse.move(0, 0);
  const input = page.getByTestId("message-input");
  await input.fill(body);
  await input.press("Enter");
  const row = committed(page, target);
  await expect(row).toHaveCount(1);
  // Reply now, while it is on screen; the composer keeps the context.
  await revealFooter(page, row);
  await row.getByTestId("message-reply-button").click();
  await expect(page.getByText(`Replying to`)).toBeVisible();
  for (let i = 0; i < 8; i++) await deliverOffscreen(page, `between ${i}: ${"y".repeat(200)}`);
  await send(page, reply);
  await expectSettledAtBottom(page, "premise: the own reply took the view to the end");
  await page.mouse.move(0, 0);
  const strip = committed(page, reply).getByTestId("reply-strip");
  await expect(strip).toBeVisible();
  const quoted = await targetOf(page, strip);
  await expect(quoted).toContainText(target);
  expect(await offView(quoted), "premise: the quoted message is above the view").toBeLessThan(0);
  return { strip, quoted };
}

/// How far `row`'s bottom sits below the history view's top (negative: the
/// row is entirely above the view).
function offView(row: Locator): Promise<number> {
  return row.evaluate((el) => {
    const view = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return el.getBoundingClientRect().bottom - view.top;
  });
}

/// `row`'s place in the history's visible area.
function placement(row: Locator) {
  return row.evaluate((el) => {
    const c = document.getElementById("chat-scroll-container")!;
    const view = c.getBoundingClientRect();
    const top = view.top + c.clientTop;
    const r = el.getBoundingClientRect();
    return {
      fromTop: r.top - top,
      centreOffset: r.top + r.height / 2 - (top + c.clientHeight / 2),
      height: r.height,
      view: c.clientHeight,
    };
  });
}

/// Every reply highlight the document still has, cancelled ones excluded.
function liveHighlights(page: Page) {
  return page.evaluate((id) =>
    document
      .getAnimations()
      .filter((a) => a.id === id)
      .map((a) => ({
        target: ((a.effect as KeyframeEffect).target as Element).id,
        playState: a.playState,
        currentTime: Number(a.currentTime),
      })),
    HIGHLIGHT,
  );
}

/// Pause the live highlight and seek it to `ms`.
function seekHighlight(page: Page, ms: number) {
  return page.evaluate(
    ({ id, ms }) => {
      const a = document.getAnimations().find((x) => x.id === id)!;
      a.pause();
      a.currentTime = ms;
    },
    { id: HIGHLIGHT, ms },
  );
}

function finishHighlight(page: Page) {
  return page.evaluate((id) => document.getAnimations().find((x) => x.id === id)!.finish(), HIGHLIGHT);
}

async function backgroundOf(row: Locator): Promise<number[]> {
  return resolveColor(row.page(), await row.evaluate((el) => getComputedStyle(el).backgroundColor));
}

/// Every scroll position on the page except the room history's.
function otherScrollPositions(page: Page) {
  return page.evaluate(() => {
    const out: string[] = [`window ${window.scrollX},${window.scrollY}`];
    for (const el of Array.from(document.querySelectorAll("*"))) {
      if (el.id === "chat-scroll-container") continue;
      if (el.scrollTop || el.scrollLeft) out.push(`${el.tagName}#${el.id}.${el.className} ${el.scrollLeft},${el.scrollTop}`);
    }
    const header = document.querySelector("header")?.getBoundingClientRect();
    out.push(`header ${header?.top},${header?.left}`);
    out.push(`history left ${document.getElementById("chat-scroll-container")!.scrollLeft}`);
    return out;
  });
}

/// Focus `target` as the keyboard does. A key press first, so the scripted
/// focus counts as keyboard focus (WebKit's Tab skips buttons).
async function keyboardFocus(page: Page, target: Locator) {
  await page.keyboard.press("Escape");
  await target.focus();
  if (!(await target.evaluate((el) => el.matches(":focus-visible")))) {
    await page.keyboard.press("Shift+Tab");
    await page.keyboard.press("Tab");
  }
  await expect
    .poll(() => target.evaluate((el) => el === document.activeElement && el.matches(":focus-visible")), {
      message: "premise: keyboard focus",
    })
    .toBe(true);
}

test.describe("The reply strip", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("is a native one-line button named by its quote", async ({ page }) => {
    await openRoomWithComposer(page);
    const strip = page.getByTestId("reply-strip").first();
    await expect(strip).toBeVisible();
    expect(await strip.evaluate((el) => [el.tagName, el.getAttribute("type")])).toEqual(["BUTTON", "button"]);
    // Native: no ARIA role or tab index standing in for one.
    for (const attr of ["role", "tabindex", "aria-label"]) await expect(strip).not.toHaveAttribute(attr, /.*/);
    // The author, then the quoted text (the example data's names vary).
    const quote = await strip.evaluate((el) => {
      const parts = Array.from(el.querySelectorAll(".reply-strip-text > span:not([aria-hidden])"));
      return parts.map((p) => (p.textContent ?? "").trim()).join(" ");
    });
    expect(quote, "premise: an author and a preview").toMatch(/^\S.* Lorem ipsum/);
    await expect(strip).toHaveAccessibleName(quote);
    await expect(strip).toHaveAttribute("title", /jump/i);
    const target = await targetOf(page, strip);
    await expect(target).toHaveCount(1);
    await expect(target).toHaveAttribute("data-testid", "message-row");
  });

  test("stays one line inside the content column when hovered or focused", async ({ page }) => {
    await page.setViewportSize({ width: 480, height: 900 });
    await openRoomWithComposer(page);
    // The self reply to the long URL: its unbreakable preview is wider than the column.
    const strip = page.getByTestId("reply-strip").filter({ hasText: "longlongurl" });
    await strip.scrollIntoViewIfNeeded();
    await page.mouse.move(0, 0);
    const measure = () =>
      strip.evaluate((el) => {
        const s = el.getBoundingClientRect();
        const column = el.closest('[data-testid="message-content"]')!.getBoundingClientRect();
        const scroller = document.getElementById("chat-scroll-container")!;
        const line = parseFloat(getComputedStyle(el).lineHeight);
        return {
          height: s.height,
          line,
          inside: s.left >= column.left - 0.5 && s.right <= column.right + 0.5,
          sideways: scroller.scrollWidth - scroller.clientWidth,
        };
      });
    const rest = await measure();
    expect(rest.height, "one line").toBeLessThan(rest.line * 1.5);
    expect(rest.inside).toBe(true);
    expect(rest.sideways).toBeLessThanOrEqual(0);
    // The preview is cut with an ellipsis, not wrapped.
    const truncation = await strip.evaluate((el) => {
      const inner = el.querySelector<HTMLElement>(".reply-strip-text")!;
      const cs = getComputedStyle(inner);
      return [cs.whiteSpace, cs.textOverflow, cs.overflowX, inner.scrollWidth > inner.clientWidth];
    });
    expect(truncation).toEqual(["nowrap", "ellipsis", "hidden", true]);

    if (!(await coarsePointer(page))) {
      await strip.hover();
      expect(await measure(), "hovered: no expansion").toEqual(rest);
    }
    await keyboardFocus(page, strip);
    expect(await measure(), "focused: no expansion").toEqual(rest);
    // A visible focus ring instead.
    const outline = await strip.evaluate((el) => {
      const cs = getComputedStyle(el);
      return [cs.outlineStyle, parseFloat(cs.outlineWidth)];
    });
    expect(outline[0]).not.toBe("none");
    expect(outline[1]).toBeGreaterThanOrEqual(2);
  });

  for (const key of ["Enter", "Space"]) {
    test(`${key} on the focused strip jumps to the original`, async ({ page }) => {
      const { strip, quoted } = await replyFarBelow(page, `keyboard target ${key}`, `keyboard reply ${key}`);
      await keyboardFocus(page, strip);
      await page.keyboard.press(key);
      await expect.poll(() => liveHighlights(page)).toEqual([expect.objectContaining({ target: await quoted.getAttribute("id") })]);
      expect(Math.abs((await placement(quoted)).centreOffset), "centred").toBeLessThanOrEqual(1);
    });
  }
});

test.describe("Jumping to the original", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("centres the original in the room history and scrolls nothing else", async ({ page }) => {
    const { strip, quoted } = await replyFarBelow(page, "centred target", "reply to the centred target");
    const others = await otherScrollPositions(page);
    await strip.click();
    await expect.poll(async () => Math.abs((await placement(quoted)).centreOffset), { message: "centred" }).toBeLessThanOrEqual(1);
    await settle(page);
    expect(await otherScrollPositions(page), "only the history moved").toEqual(others);
  });

  test("top-aligns an original taller than the view, with clearance", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 520 });
    const paragraphs = Array.from({ length: 30 }, (_, i) => `tall target paragraph ${i}`).join("\n\n");
    const { strip, quoted } = await replyFarBelow(page, "tall target paragraph 29", "reply to the tall target", paragraphs);
    const before = await placement(quoted);
    expect(before.height, "premise: taller than the view").toBeGreaterThan(before.view);
    const others = await otherScrollPositions(page);
    await strip.click();
    await expect.poll(async () => (await placement(quoted)).fromTop).toBeCloseTo(TALL_CLEARANCE_PX, 0);
    await settle(page);
    expect(await otherScrollPositions(page), "only the history moved").toEqual(others);
  });

  test("an original that is not rendered is a no-op", async ({ page }) => {
    await openRoomWithComposer(page);
    await page.mouse.move(0, 0);
    await send(page, "unrendered target");
    const row = committed(page, "unrendered target");
    await revealFooter(page, row);
    await row.getByTestId("message-reply-button").click();
    // Past the render window of a freshly opened room (60 items), under the
    // room's 100-message cap so the original stays in the room. One at a
    // time, so each is its own item (a batch shares a timestamp, and its two
    // test authors interleave into shared groups).
    for (let i = 0; i < 62; i++) await callRiverTest(page, "appendMessage", `window filler ${i}`);
    await expect(committed(page, "window filler 61")).toHaveCount(1);
    await send(page, "reply to the unrendered target");
    // Reopening renders the initial window only.
    await callRiverTest(page, "switchRoom", "Public Discussion Room");
    await expect(page.getByRole("heading", { name: "Public Discussion Room" })).toBeVisible();
    await callRiverTest(page, "switchRoom", "Your Private Room");
    const strip = committed(page, "reply to the unrendered target").getByTestId("reply-strip");
    await expect(strip).toBeVisible();
    await expectSettledAtBottom(page, "premise: reopened at the end");
    await expect(page.locator(`[id="${await strip.getAttribute("data-reply-target")}"]`), "premise: the original is not rendered").toHaveCount(0);
    await expect(committed(page, "unrendered target").filter({ hasNotText: "reply to" })).toHaveCount(0);

    const before = await scrollMetrics(page);
    const others = await otherScrollPositions(page);
    await strip.click();
    await settle(page);
    expect((await scrollMetrics(page)).top, "the history did not move").toBe(before.top);
    expect(await otherScrollPositions(page)).toEqual(others);
    expect(await liveHighlights(page), "nothing highlighted").toEqual([]);
  });

  test("a jump away releases the end: arrivals and a taller composer leave the reader there", async ({ page }) => {
    const { strip, quoted } = await replyFarBelow(page, "parked target", "reply to the parked target");
    await strip.click();
    await expect.poll(async () => Math.abs((await placement(quoted)).centreOffset)).toBeLessThanOrEqual(1);
    await settle(page);
    await expectParkedAwayFromEnd(page);
    const parked = (await scrollMetrics(page)).top;
    const at = (await placement(quoted)).fromTop;

    await deliverOffscreen(page, "arrival while parked at the original");
    await settle(page);
    expect((await scrollMetrics(page)).top, "the arrival was not followed").toBe(parked);
    await expect(page.getByTestId("scroll-to-bottom")).toBeVisible();

    // The composer growing takes the bottom of the view; outside the end
    // hold nothing corrects that, so the view's top stays.
    await page.getByTestId("message-input").fill("line\n".repeat(6));
    await settle(page);
    expect((await scrollMetrics(page)).top, "no hold took the view back to the end").toBe(parked);
    expect((await placement(quoted)).fromTop).toBeCloseTo(at, 0);
  });

  test("a jump to an original already in view at the end keeps the end, and arrivals follow", async ({ page }) => {
    await openRoomWithComposer(page);
    await page.mouse.move(0, 0);
    await expectSettledAtBottom(page, "premise: opened at the end");
    // The self reply to the long URL quotes the message just above it.
    const strip = page.getByTestId("reply-strip").filter({ hasText: "longlongurl" });
    const quoted = await targetOf(page, strip);
    expect(await offView(quoted), "premise: the original is in view").toBeGreaterThan(0);
    await strip.click();
    await expect.poll(() => liveHighlights(page)).toHaveLength(1);
    await settle(page);
    expect(await distanceFromBottom(page), "still at the end").toBeLessThanOrEqual(4);
    await deliverOffscreen(page, "arrival after a jump at the end");
    await expectFollowed(page, "arrival after a jump at the end", "the reader at the end follows");
  });

  test("the reader's row stays put when the member panel rewraps the rows after a jump", async ({ page }) => {
    await page.setViewportSize({ width: 820, height: 768 });
    const { strip } = await replyFarBelow(page, "reflow target", "reply to the reflow target");
    await strip.click();
    await settle(page);
    await expectParkedAwayFromEnd(page);
    const row = await readingRow(page);
    expect(row, "premise: a row is in view").not.toBeNull();
    const height = (await scrollMetrics(page)).height;
    await page.getByTestId("members-panel-toggle").click();
    await expect.poll(async () => (await scrollMetrics(page)).height, { message: "premise: the rows rewrap" }).not.toBe(height);
    await settle(page);
    const after = await page.evaluate((key) => {
      const c = document.getElementById("chat-scroll-container")!;
      const el = c.querySelector(`[data-item-key="${key}"]`);
      return el ? el.getBoundingClientRect().top - c.getBoundingClientRect().top : null;
    }, row!.key);
    expect(after).not.toBeNull();
    expect(Math.abs(after! - row!.top)).toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
  });
});

test.describe("The highlight", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("paints the whole row in the surface colour for two seconds, with no fade", async ({ page }) => {
    const { strip, quoted } = await replyFarBelow(page, "painted target", "reply to the painted target");
    const surface = await resolveColor(page, "var(--color-surface)");
    await strip.click();
    await expect.poll(() => liveHighlights(page)).toHaveLength(1);
    // On the row itself, so the avatar gutter, header and footer all carry it.
    expect((await liveHighlights(page))[0].target).toBe(await quoted.getAttribute("id"));
    expect(
      await quoted.evaluate((el) =>
        [".msg-gutter", '[data-testid="message-content"]', '[data-testid="message-footer"]'].map(
          (sel) => getComputedStyle(el.querySelector(sel)!).backgroundColor,
        ),
      ),
      "nothing inside the row paints over it",
    ).toEqual(["rgba(0, 0, 0, 0)", "rgba(0, 0, 0, 0)", "rgba(0, 0, 0, 0)"]);
    const timing = await page.evaluate((id) => {
      const effect = document.getAnimations().find((a) => a.id === id)!.effect as KeyframeEffect;
      const t = effect.getComputedTiming();
      return { duration: t.duration, endTime: t.endTime, fill: t.fill };
    }, HIGHLIGHT);
    expect(timing).toEqual({ duration: 2000, endTime: 2000, fill: "none" });
    // Full strength from the first moment to the last.
    for (const ms of [0, 1000, 1999]) {
      await seekHighlight(page, ms);
      expect(await backgroundOf(quoted), `at ${ms}ms`).toEqual(surface);
    }
  });

  test("ends on the row's own background at once: its tint, and the hover band under the pointer", async ({ page }) => {
    const { strip, quoted } = await replyFarBelow(page, "restored target", "reply to the restored target");
    await expect(quoted).toHaveAttribute("data-message-self", "true");
    const hover = !(await coarsePointer(page));
    const underlying = await resolveColor(page, hover ? "var(--color-row-self-hover)" : "var(--color-row-self)");
    const surface = await resolveColor(page, "var(--color-surface)");
    expect(underlying, "premise: the highlight differs from the row").not.toEqual(surface);
    await strip.click();
    await expect.poll(() => liveHighlights(page)).toHaveLength(1);
    await seekHighlight(page, 1999);
    if (hover) {
      const box = (await quoted.boundingBox())!;
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await settle(page);
    }
    expect(await backgroundOf(quoted), "the highlight covers the row's own background").toEqual(surface);
    await finishHighlight(page);
    // No transition or leftover animation eases it back.
    expect(await quoted.evaluate((el) => el.getAnimations().length), "nothing left animating").toBe(0);
    expect(await backgroundOf(quoted)).toEqual(underlying);
  });

  test("a second jump restarts it, and only one row is ever highlighted", async ({ page }) => {
    const { strip, quoted } = await replyFarBelow(page, "restarted target", "reply to the restarted target");
    const id = await quoted.getAttribute("id");
    await strip.click();
    await expect.poll(() => liveHighlights(page)).toHaveLength(1);
    await seekHighlight(page, 1500);
    await page.evaluate((h) => {
      (window as unknown as { first: Animation }).first = document.getAnimations().find((a) => a.id === h)!;
    }, HIGHLIGHT);

    // The same row again: a fresh two seconds, the earlier one cancelled.
    await strip.scrollIntoViewIfNeeded();
    await strip.click();
    await expect.poll(() => page.evaluate(() => (window as unknown as { first: Animation }).first.playState)).toBe("idle");
    const again = await liveHighlights(page);
    expect(again).toHaveLength(1);
    expect(again[0].target).toBe(id);
    expect(again[0].currentTime).toBeLessThan(1000);

    // Another original: the previous row's highlight ends at once.
    const other = page.getByTestId("reply-strip").filter({ hasText: "longlongurl" });
    await other.scrollIntoViewIfNeeded();
    await other.click();
    const otherTarget = await (await targetOf(page, other)).getAttribute("id");
    await expect.poll(() => liveHighlights(page)).toEqual([expect.objectContaining({ target: otherTarget })]);
  });
});
