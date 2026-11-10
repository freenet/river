import { test, expect, Locator, Page } from "@playwright/test";
import {
  asTouchTablet,
  clickFooter,
  coarsePointer,
  committedRow,
  FOOTER_CONTROLS,
  inlineFooter,
  keyboardFocus,
  openRoomWithComposer,
  reactTo,
  revealFooter,
  settle,
  submitMessage,
} from "./example-room";
import { callRiverTest } from "./river-test";

// Every signed message ends with a footer: on the left its reaction chips and
// its one React, then a trailing cluster of Reply, and Edit and Delete on your
// own messages. The footer's room is kept for every message (a line of its
// own, or on a desktop with a mouse and no reactions the end of its last line:
// message-actions-inline.spec.ts), so showing its controls never moves
// anything. A fine pointer shows them while the row is
// hovered, holds keyboard focus or has its picker open, and shows a control
// that has focus however it got it; a touch or hybrid pointer shows them at
// rest. Reaction chips always show. Below 48rem the trailing cluster is
// hidden and a tap on the row opens it in the actions sheet
// (mobile-message-actions.spec.ts); a phone runs the desktop footer's checks
// at a touch tablet's width.

/// A received message (a fixed test identity, inserted before the newest)
/// and an own one (sent from the composer), neither with reactions.
async function openRoomWithBoth(page: Page): Promise<{ received: Locator; own: Locator }> {
  await openRoomWithComposer(page);
  await callRiverTest(page, "insertMessageBeforeLast", "received footer");
  const own = await submitMessage(page, "own footer");
  await expect(own).toHaveCount(1);
  return { received: committedRow(page, "received footer"), own };
}

/// The test ids of a container's direct controls, in order.
function testIds(container: Locator): Promise<string[]> {
  return container.evaluate((el) =>
    Array.from(el.querySelectorAll("button")).map((b) => b.getAttribute("data-testid") ?? "?"),
  );
}

/// Whether each control is shown and takes a hit at its centre.
function controlStates(row: Locator, ids: string[]) {
  return row.evaluate(async (el, ids) => {
    const out: Record<string, { opacity: string; hit: boolean }> = {};
    for (const id of ids) {
      const b = el.querySelector(`[data-testid="${id}"]`) as HTMLElement;
      await Promise.all(b.getAnimations().map((a) => a.finished));
      const r = b.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      out[id] = { opacity: getComputedStyle(b).opacity, hit: !!at && b.contains(at) };
    }
    return out;
  }, ids);
}

function allStates(ids: string[], state: { opacity: string; hit: boolean }) {
  return Object.fromEntries(ids.map((id) => [id, state]));
}

const SHOWN = { opacity: "1", hit: true };
const HIDDEN = { opacity: "0", hit: false };

function rowBox(row: Locator) {
  return row.evaluate((el) => {
    const r = el.getBoundingClientRect();
    const f = el.querySelector('[data-testid="message-footer"]')!.getBoundingClientRect();
    return { height: r.height, footerTop: f.top - r.top, footerHeight: f.height };
  });
}

test.describe("Message footer", () => {
  test("every signed message has one footer: chips and one React, then Reply, with Edit and Delete only on your own", async ({ page }) => {
    await asTouchTablet(page);
    const { received, own } = await openRoomWithBoth(page);

    for (const [row, cluster] of [
      [received, ["message-reply-button"]],
      [own, ["message-reply-button", "message-edit-button", "message-delete-button"]],
    ] as const) {
      const footer = row.getByTestId("message-footer");
      await expect(footer).toHaveCount(1);
      expect(await testIds(footer.getByTestId("message-reaction-row"))).toEqual(["message-react-button"]);
      expect(await testIds(footer.getByTestId("message-action-cluster"))).toEqual(cluster);
      await expect(footer.locator("time"), "no time in the footer").toHaveCount(0);
      for (const id of cluster.concat("message-react-button")) {
        await expect(footer.getByTestId(id)).toHaveAttribute("type", "button");
      }
    }
    // Accessible names, each also the tooltip.
    for (const [id, name] of [
      ["message-react-button", "React"],
      ["message-reply-button", "Reply"],
      ["message-edit-button", "Edit message"],
      ["message-delete-button", "Delete message"],
    ]) {
      const control = own.getByTestId(id);
      await expect(control).toHaveAccessibleName(name);
      await expect(control).toHaveAttribute("title", name);
    }
    await expect(received.getByTestId("message-edit-button")).toHaveCount(0);
    await expect(received.getByTestId("message-delete-button")).toHaveCount(0);

    // Every message in the room: exactly one way to react and one Reply.
    const perRow = await page.getByTestId("message-row").evaluateAll((rows) =>
      rows.map((r) => [
        r.querySelectorAll('button[aria-label="React"]').length,
        r.querySelectorAll('[data-testid="message-react-button"]').length,
        r.querySelectorAll('[data-testid="message-reply-button"]').length,
      ]),
    );
    expect(perRow.length, "premise: the room has messages").toBeGreaterThan(3);
    for (const counts of perRow) expect(counts).toEqual([1, 1, 1]);
  });

  test("Reply, Edit and Delete act from the footer", async ({ page }) => {
    const { received, own } = await openRoomWithBoth(page);

    // Reply: the composer quotes the message, and Cancel clears it.
    await clickFooter(page, received, "message-reply-button");
    const composer = page.getByTestId("message-composer");
    await expect(composer).toContainText("Replying to");
    await expect(composer).toContainText("received footer");
    await page.getByTitle("Cancel reply").click();
    await expect(composer).not.toContainText("Replying to");

    // Edit: Escape cancels, Save changes the message.
    await clickFooter(page, own, "message-edit-button");
    const editArea = page.locator('textarea[id^="edit-msg-"]');
    await expect(editArea).toBeVisible();
    await expect(own.getByTestId("message-action-cluster"), "no actions on a row being edited").toHaveCount(0);
    await editArea.press("Escape");
    await expect(editArea).toHaveCount(0);
    await clickFooter(page, own, "message-edit-button");
    await editArea.fill("own footer, edited");
    await page.getByTestId("edit-save-button").click();
    const edited = committedRow(page, "own footer, edited");
    await expect(edited).toHaveCount(1);

    // Delete asks first: Cancel keeps the message, Delete removes it.
    await clickFooter(page, edited, "message-delete-button");
    const heading = page.getByRole("heading", { name: /Delete Message/ });
    await expect(heading).toBeVisible();
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(heading).toHaveCount(0);
    await expect(edited).toHaveCount(1);
    await clickFooter(page, edited, "message-delete-button");
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await expect(edited).toHaveCount(0);
  });

  test("a footer action that cannot be signed is rolled back, and says so", { tag: "@chromium-only" }, async ({ page }) => {
    const { own } = await openRoomWithBoth(page);
    await callRiverTest(page, "holdMessageSigning");
    await clickFooter(page, own, "message-delete-button");
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await expect(own, "the delete shows while its signature is held").toHaveCount(0);
    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(own, "the failed delete is rolled back").toHaveCount(1);
    await expect(page.getByTestId("toast-message")).toHaveText("Your delete could not be sent");
    // The restored row has its footer back.
    await revealFooter(page, own);
    await expect(own.getByTestId("message-delete-button")).toBeVisible();
  });
});

test.describe("Footer controls on a fine pointer", () => {
  test.beforeEach(async ({ page }) => test.skip(await coarsePointer(page), "shown at rest on a touch pointer"));

  test("hidden and hit-transparent at rest; hover, keyboard focus and an open picker show them; the row never moves", async ({ page }) => {
    const { own } = await openRoomWithBoth(page);
    const ids = FOOTER_CONTROLS;
    await own.scrollIntoViewIfNeeded();
    await page.mouse.move(0, 0);
    await settle(page);
    const rest = await rowBox(own);
    // Where the controls' room is kept (a footer line, or on a desktop with a
    // mouse the end of the last line) is message-actions-inline.spec.ts's.
    expect(await controlStates(own, ids), "at rest").toEqual(allStates(ids, HIDDEN));

    await own.getByTestId("message-body").hover();
    await expect.poll(() => controlStates(own, ids), { message: "hovered" }).toEqual(allStates(ids, SHOWN));
    expect(await rowBox(own), "showing them moves nothing").toEqual(rest);

    await page.mouse.move(0, 0);
    await expect.poll(() => controlStates(own, ids)).toEqual(allStates(ids, HIDDEN));

    // Keyboard focus on any of them shows all of them.
    await keyboardFocus(page, own.getByTestId("message-reply-button"));
    await expect.poll(() => controlStates(own, ids), { message: "keyboard focus" }).toEqual(allStates(ids, SHOWN));
    await keyboardFocus(page, own.getByTestId("message-react-button"));
    expect(await controlStates(own, ids)).toEqual(allStates(ids, SHOWN));
    expect(await rowBox(own)).toEqual(rest);

    // An open picker keeps them shown after the pointer leaves the row and
    // focus leaves it.
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("emoji-picker")).toBeVisible();
    await expect(own.getByTestId("message-react-button")).toHaveAttribute("aria-expanded", "true");
    await own.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.mouse.move(0, 0);
    expect(await own.evaluate((el) => el.matches(":hover")), "premise: the row is not hovered").toBe(false);
    expect(await own.evaluate((el) => el.matches(":has(:focus-visible)")), "premise: nothing focused in it").toBe(false);
    const shown = await own.evaluate(async (el) => {
      const b = el.querySelector('[data-testid="message-react-button"]') as HTMLElement;
      await Promise.all(b.getAnimations().map((a) => a.finished));
      return getComputedStyle(b).opacity;
    });
    expect(shown, "shown while its picker is open").toBe("1");
  });

  test("reaction chips show at rest", async ({ page }) => {
    const { own } = await openRoomWithBoth(page);
    const chip = await reactTo(page, own);
    // The pick returned focus to React where the engine focuses on a click,
    // and a focused React stays shown; at rest is with focus gone too.
    await own.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.mouse.move(0, 0);
    await settle(page);
    expect(await chip.evaluate((el) => getComputedStyle(el).opacity)).toBe("1");
    await expect(chip).toBeVisible();
    expect(await controlStates(own, ["message-react-button"])).toEqual(allStates(["message-react-button"], HIDDEN));
  });

  test("a mouse click inside the row does not keep them after the pointer leaves", async ({ page }) => {
    await openRoomWithComposer(page);
    const row = page.getByTestId("message-row").filter({ has: page.getByTestId("reply-strip") }).first();
    await row.scrollIntoViewIfNeeded();
    const strip = row.getByTestId("reply-strip");
    // A mouse click focuses the strip, a native button, in Chromium; WebKit
    // focuses no button on a click, so there the focus comes from script
    // right after it. Either way it is not keyboard focus.
    await strip.click();
    if (!(await strip.evaluate((el) => el === document.activeElement))) await strip.evaluate((el) => (el as HTMLElement).focus());
    await expect
      .poll(() => strip.evaluate((el) => el === document.activeElement && !el.matches(":focus-visible")), {
        message: "premise: the strip has focus, not keyboard focus",
      })
      .toBe(true);
    await page.mouse.move(0, 0);
    await expect.poll(() => controlStates(row, ["message-reply-button"]), { message: "pointer gone, focus from a mouse click" }).toEqual(
      allStates(["message-reply-button"], HIDDEN),
    );
    // The same focus from the keyboard does show them.
    await strip.evaluate((el) => (el as HTMLElement).blur());
    await keyboardFocus(page, strip);
    await expect.poll(() => controlStates(row, ["message-reply-button"])).toEqual(allStates(["message-reply-button"], SHOWN));
  });
});

test.describe("Footer controls on a touch pointer", () => {
  test.beforeEach(async ({ page }) => {
    test.skip(!(await coarsePointer(page)), "hidden at rest on a fine pointer");
    // A touch tablet: wide enough for the trailing cluster.
    await asTouchTablet(page);
  });

  test("shown and tappable at rest", async ({ page }) => {
    const { received, own } = await openRoomWithBoth(page);
    await own.scrollIntoViewIfNeeded();
    expect(await controlStates(own, FOOTER_CONTROLS)).toEqual(allStates(FOOTER_CONTROLS, SHOWN));
    const ids = ["message-react-button", "message-reply-button"];
    await received.scrollIntoViewIfNeeded();
    expect(await controlStates(received, ids)).toEqual(allStates(ids, SHOWN));
  });
});

/// Every chip and footer control of `row`, with where a hit at its centre and
/// 1px inside each corner lands, and the boxes it must stay clear of.
function footerLayout(row: Locator) {
  return row.evaluate((el) => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width, height: r.height };
    };
    const hits = (e: Element) => {
      const r = e.getBoundingClientRect();
      const points = [
        [r.left + r.width / 2, r.top + r.height / 2],
        [r.left + 1, r.top + 1],
        [r.right - 1, r.top + 1],
        [r.left + 1, r.bottom - 1],
        [r.right - 1, r.bottom - 1],
      ];
      return points.filter(([x, y]) => {
        const at = document.elementFromPoint(x, y);
        return !(at && e.contains(at));
      });
    };
    const left = Array.from(el.querySelectorAll('[data-testid="message-reaction-row"] > *'))
      .map((wrapper) => wrapper.matches("button") ? wrapper : wrapper.querySelector('[data-testid="reaction-chip"]'))
      .filter((e): e is Element => !!e)
      .map((e) => ({ id: e.getAttribute("data-testid")!, box: box(e), misses: e.matches("button") ? hits(e) : [] }));
    const cluster = Array.from(el.querySelectorAll('[data-testid="message-action-cluster"] button')).map((e) => ({
      id: e.getAttribute("data-testid")!,
      box: box(e),
      misses: hits(e),
    }));
    const chipCentreMisses = Array.from(el.querySelectorAll('[data-testid="reaction-chip"]')).filter((c) => {
      const r = c.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return !(at && c.contains(at));
    }).length;
    const body = el.querySelector('[data-testid="message-body"]')!;
    const bodyText: Box[] = [];
    const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT);
    for (let t = walker.nextNode(); t; t = walker.nextNode()) {
      if (!t.textContent?.trim()) continue;
      const range = document.createRange();
      range.selectNodeContents(t);
      for (const r of Array.from(range.getClientRects())) {
        if (r.width > 0 && r.height > 0) {
          bodyText.push({ left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width, height: r.height });
        }
      }
    }
    return {
      left,
      cluster,
      chipCentreMisses,
      body: box(body),
      bodyText,
      content: box(el.querySelector('[data-testid="message-content"]')!),
      scroller: box(document.getElementById("chat-scroll-container")!),
      viewport: document.documentElement.clientWidth,
    };
  });
}

type Box = { left: number; right: number; top: number; bottom: number; width: number; height: number };
const overlaps = (a: Box, b: Box) =>
  Math.min(a.right, b.right) - Math.max(a.left, b.left) > 0.5 && Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top) > 0.5;

function expectUsableFooter(layout: Awaited<ReturnType<typeof footerLayout>>, what: string, inline = false) {
  const all = [...layout.left, ...layout.cluster];
  // React follows the chips; the cluster is Reply, Edit, Delete.
  expect(layout.left.at(-1)!.id, `${what}: React ends the left group`).toBe("message-react-button");
  expect(layout.cluster.map((c) => c.id), `${what}: the trailing cluster`).toEqual([
    "message-reply-button",
    "message-edit-button",
    "message-delete-button",
  ]);
  // Reply, Edit and Delete stay together on one line, in order.
  const tops = layout.cluster.map((c) => Math.round(c.box.top));
  expect(new Set(tops).size, `${what}: the cluster is one line ${tops}`).toBe(1);
  for (let i = 1; i < layout.cluster.length; i++) {
    expect(layout.cluster[i].box.left, `${what}: ${layout.cluster[i].id} after ${layout.cluster[i - 1].id}`).toBeGreaterThanOrEqual(
      layout.cluster[i - 1].box.right - 0.5,
    );
  }
  // React after its last chip: on the same line to its right, or below it.
  const chips = layout.left.slice(0, -1);
  const react = layout.left.at(-1)!.box;
  if (chips.length) {
    const last = chips.at(-1)!.box;
    const sameLine = react.top < last.bottom && react.bottom > last.top;
    expect(sameLine ? react.left >= last.right - 0.5 : react.top >= last.bottom - 0.5, `${what}: React follows the chips`).toBe(true);
  }
  // Inline controls use 2px gaps; a separate footer line keeps chips 16px clear.
  const first = layout.cluster[0].box;
  const sharing = layout.left.filter((c) => c.box.top < first.bottom && c.box.bottom > first.top);
  if (sharing.length) {
    const end = Math.max(...sharing.map((c) => c.box.right));
    expect(first.left - end, `${what}: the gap before the cluster`).toBeGreaterThanOrEqual((inline ? 2 : 16) - 0.5);
  }
  // Targets of the footer's size, none overlapping another or the message.
  for (const c of [layout.left.at(-1)!, ...layout.cluster]) {
    expect(c.box.width, `${what}: ${c.id} width`).toBeGreaterThanOrEqual(26);
    expect(c.box.height, `${what}: ${c.id} height`).toBeGreaterThanOrEqual(26);
    expect(c.misses, `${what}: a hit anywhere in ${c.id} reaches it`).toEqual([]);
  }
  expect(layout.chipCentreMisses, `${what}: every chip takes a hit at its centre`).toBe(0);
  if (inline) expect(layout.bodyText.length, `${what}: premise: the message has rendered text`).toBeGreaterThan(0);
  const messageBoxes = inline ? layout.bodyText : [layout.body];
  for (let i = 0; i < all.length; i++) {
    for (const b of messageBoxes) {
      expect(overlaps(all[i].box, b), `${what}: ${all[i].id} clear of the message`).toBe(false);
    }
    for (let j = i + 1; j < all.length; j++) {
      expect(overlaps(all[i].box, all[j].box), `${what}: ${all[i].id} #${i} and ${all[j].id} #${j} overlap`).toBe(false);
    }
    // Inside the content column and the screen.
    expect(all[i].box.left).toBeGreaterThanOrEqual(layout.content.left - 0.5);
    expect(all[i].box.right).toBeLessThanOrEqual(Math.min(layout.content.right, layout.scroller.right, layout.viewport) + 0.5);
  }
}

test.describe("Footer at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  // Below 48rem the footer is the chips and the +: Reply, Edit and Delete are
  // a tap on the row away, in the actions sheet.
  test("chips wrap with React after them, no trailing cluster, and every target takes its own hits", async ({ page }) => {
    await openRoomWithComposer(page);
    const bare = await submitMessage(page, "narrow bare");
    await expect(bare).toHaveCount(1);
    const row = await submitMessage(page, "narrow chips");
    await expect(row).toHaveCount(1);
    await callRiverTest(page, "addReactionsToNewest", ["👍", "❤️", "😂", "🔥", "🎉", "👀", "😮", "😢", "🙏", "💯"]);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(10);

    for (const [what, target] of [
      ["no chips", bare],
      ["wrapped chips", row],
    ] as const) {
      await target.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await settle(page);
      const layout = await footerLayout(target);
      expect(
        layout.cluster.filter((c) => c.box.width > 0 || c.box.height > 0),
        `${what}: no visible trailing cluster`,
      ).toEqual([]);
      if (what === "wrapped chips") {
        const lines = new Set(layout.left.filter((c) => c.id === "reaction-chip").map((c) => Math.round(c.box.top)));
        expect(lines.size, "premise: the chips wrap").toBeGreaterThan(1);
        expect(await testIds(target.getByTestId("message-reaction-row"))).toEqual(["message-react-button"]);
        // React after its last chip: on the same line to its right, or below it.
        const last = layout.left.at(-2)!.box;
        const react = layout.left.at(-1)!.box;
        const sameLine = react.top < last.bottom && react.bottom > last.top;
        expect(sameLine ? react.left >= last.right - 0.5 : react.top >= last.bottom - 0.5, "React follows the chips").toBe(true);
      } else {
        expect(layout.left.map((c) => c.id), "premise: no chips").toEqual(["message-react-button"]);
      }
      const react = layout.left.at(-1)!;
      expect(react.id).toBe("message-react-button");
      expect(react.box.width).toBeGreaterThanOrEqual(26);
      expect(react.box.height).toBeGreaterThanOrEqual(26);
      expect(react.misses, `${what}: a hit anywhere in React reaches it`).toEqual([]);
      expect(layout.chipCentreMisses).toBe(0);
      for (const c of layout.left) {
        expect(overlaps(c.box, layout.body), `${what}: ${c.id} clear of the message`).toBe(false);
        expect(c.box.right).toBeLessThanOrEqual(Math.min(layout.content.right, layout.scroller.right, layout.viewport) + 0.5);
      }
    }
  });
});

test.describe("Footer at 768px", () => {
  test.use({ viewport: { width: 768, height: 700 } });

  // The narrowest desktop footer: React after the chips, the cluster whole.
  test("chips wrap with React after them, the cluster stays whole, and every target takes its own hits", async ({ page }) => {
    await openRoomWithComposer(page);
    const bare = await submitMessage(page, "tablet bare");
    await expect(bare).toHaveCount(1);
    const row = await submitMessage(page, "tablet chips");
    await expect(row).toHaveCount(1);
    await callRiverTest(page, "addReactionsToNewest", ["👍", "❤️", "😂", "🔥", "🎉", "👀", "😮", "😢", "🙏", "💯"]);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(10);
    for (const [what, target] of [
      ["no chips", bare],
      ["chips", row],
    ] as const) {
      await target.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await revealFooter(page, target);
      await settle(page);
      expectUsableFooter(await footerLayout(target), what, what === "no chips" && (await inlineFooter(page)));
    }
    await clickFooter(page, row, "message-react-button");
    await expect(page.getByTestId("emoji-picker")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("emoji-picker")).toBeHidden();
    await clickFooter(page, row, "message-reply-button");
    await expect(page.getByTestId("message-composer")).toContainText("Replying to");
  });
});
