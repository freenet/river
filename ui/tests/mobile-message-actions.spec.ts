import { test, expect, Locator, Page } from "@playwright/test";
import {
  coarsePointer,
  committedRow,
  hasTouch,
  keyboardFocus,
  narrowLayout,
  openMessageActions,
  openRoomWithComposer,
  plainPointOf,
  press,
  settle,
  submitMessage,
  tapRow,
  touchmovePrevented,
} from "./example-room";
import { DM_PEER, openThread } from "./dm-thread";
import { deliver, scrollMetrics } from "./history-geometry";
import { callRiverTest } from "./river-test";

// Below Tailwind's `md` (48rem) a signed message's trailing Reply, Edit and
// Delete are hidden; a tap on the row opens them, and only them, in a modal
// actions sheet (mobile_message_actions.rs), and its one reaction + stays
// visible and opens a separate reaction popup (mobile-reaction-input.spec.ts).
// Width alone decides: a narrow mouse window is mobile, a wide touch screen
// keeps the desktop footer. These run on every project at the widths they
// set, so the desktop projects are narrow mouse windows here.

const SHEET = "mobile-message-actions-sheet";
const POPUP = "mobile-reaction-popup";

function sheet(page: Page): Locator {
  return page.getByTestId(SHEET);
}

/// Whether nothing opened: neither mobile surface, and no tap reached them.
async function expectNoSurface(page: Page) {
  await settle(page);
  // Past the double-click window a mouse click waits out before opening.
  await page.waitForTimeout(400);
  await expect(sheet(page)).toBeHidden();
  await expect(page.getByTestId(POPUP)).toBeHidden();
}

/// A point over the history's top, below the header.
async function historyPoint(page: Page): Promise<{ x: number; y: number }> {
  const view = (await page.locator("#chat-scroll-container").boundingBox())!;
  return { x: view.x + 40, y: view.y + 30 };
}

/// A point inside the open panel of dialog `id`, on no control.
async function panelPoint(page: Page, id: string): Promise<{ x: number; y: number }> {
  const box = (await page.locator(`#${id} .mobile-sheet-panel`).boundingBox())!;
  return { x: box.x + box.width / 2, y: box.y + 6 };
}

function rowKey(row: Locator): Promise<string> {
  return row.getAttribute("id").then((id) => id!.replace(/^msg-/, ""));
}

/// The footer's state below 48rem: the cluster's display, the + per row and
/// whether it takes a hit at its centre, and the hidden entry's size.
function narrowFooter(row: Locator) {
  return row.evaluate(async (el) => {
    const react = el.querySelector('[data-testid="message-react-button"]') as HTMLElement;
    await Promise.all(react.getAnimations().map((a) => a.finished));
    const r = react.getBoundingClientRect();
    const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    const cluster = el.querySelector('[data-testid="message-action-cluster"]') as HTMLElement | null;
    const entry = el.querySelector('[data-testid="mobile-message-actions-entry"]') as HTMLElement;
    const e = entry.getBoundingClientRect();
    return {
      reacts: el.querySelectorAll('[data-testid="message-react-button"]').length,
      reactOpacity: getComputedStyle(react).opacity,
      reactHit: !!at && react.contains(at),
      clusterDisplay: cluster ? getComputedStyle(cluster).display : "absent",
      clusterHits: cluster
        ? Array.from(cluster.querySelectorAll("button")).some((b) => {
            const br = b.getBoundingClientRect();
            return br.width > 0 && br.height > 0;
          })
        : false,
      entrySize: [Math.round(e.width), Math.round(e.height)],
    };
  });
}

/// Every control a Tab press can reach in the document, by test id.
function tabbableIds(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll<HTMLElement>("button, a[href], input, textarea, select, [tabindex]"))
      .filter((e) => e.tabIndex >= 0 && !(e as HTMLButtonElement).disabled && e.getClientRects().length > 0)
      .map((e) => e.getAttribute("data-testid") ?? ""),
  );
}

/// A received message and an own one, neither with reactions; the own one
/// followed by a continuation of it.
async function openRoomWithRows(page: Page) {
  await openRoomWithComposer(page);
  await callRiverTest(page, "insertMessageBeforeLast", "received row");
  const own = await submitMessage(page, "own first row");
  await expect(own).toHaveCount(1);
  const continuation = await submitMessage(page, "own continuation row");
  await expect(continuation).toHaveCount(1);
  await page.getByTestId("message-input").blur();
  return { received: committedRow(page, "received row"), own, continuation };
}

test.describe("Below 48rem", () => {
  test.use({ viewport: { width: 390, height: 760 } });

  test("Reply, Edit and Delete leave the footer, its hits and the tab order; one + per row stays visible", async ({ page }) => {
    const { received, own, continuation } = await openRoomWithRows(page);
    expect(await narrowLayout(page), "premise: below 48rem").toBe(true);
    for (const row of [received, own, continuation]) {
      await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await page.mouse.move(0, 0);
      expect(await narrowFooter(row)).toEqual({
        reacts: 1,
        reactOpacity: "1",
        reactHit: true,
        clusterDisplay: "none",
        clusterHits: false,
        entrySize: [1, 1],
      });
    }
    const tabbable = await tabbableIds(page);
    for (const id of ["message-reply-button", "message-edit-button", "message-delete-button"]) {
      expect(tabbable, `${id} is no tab stop`).not.toContain(id);
    }
    expect(tabbable, "the + stays a tab stop").toContain("message-react-button");
    expect(tabbable, "and so does each row's actions entry").toContain("mobile-message-actions-entry");
    // Every signed row: exactly one way to react and one way into actions.
    const perRow = await page.getByTestId("message-row").evaluateAll((rows) =>
      rows.map((r) => [
        r.querySelectorAll('[data-testid="message-react-button"]').length,
        r.querySelectorAll('[data-testid="mobile-message-actions-entry"]').length,
      ]),
    );
    expect(perRow.length).toBeGreaterThan(3);
    for (const counts of perRow) expect(counts).toEqual([1, 1]);
  });

  test("a tap on a row opens actions for that exact message, and nothing else", async ({ page }) => {
    const { received, own, continuation } = await openRoomWithRows(page);
    for (const [row, own_] of [
      [received, false],
      [own, true],
      [continuation, true],
    ] as const) {
      await openMessageActions(page, row);
      const s = sheet(page);
      await expect(s).toHaveAttribute("data-target-row", await rowKey(row));
      await expect(s.getByRole("heading", { name: "Message actions" })).toBeVisible();
      const actions = await s.locator(".mobile-sheet-actions button").evaluateAll((bs) => bs.map((b) => b.getAttribute("data-testid")));
      expect(actions).toEqual(
        own_
          ? ["mobile-message-action-reply", "mobile-message-action-edit", "mobile-message-action-delete"]
          : ["mobile-message-action-reply"],
      );
      // Actions only: no reaction, shortcut or field in it.
      await expect(s.locator("input, textarea")).toHaveCount(0);
      await expect(s.getByText(/React|palette|👍/)).toHaveCount(0);
      await expect(page.getByTestId(POPUP)).toBeHidden();
      await expect(row.getByTestId("message-react-button")).toHaveAttribute("aria-expanded", "false");
      await expect(row.getByTestId("mobile-message-actions-entry")).toHaveAttribute("aria-expanded", "true");
      // Not the keyboard: a button has focus.
      await expect(s.getByTestId("mobile-message-action-reply")).toBeFocused();
      // Close returns focus to the row's entry, without scrolling.
      const before = (await scrollMetrics(page)).top;
      await press(page, s.getByTestId("mobile-message-actions-close"));
      await expect(s).toBeHidden();
      await expect(row.getByTestId("mobile-message-actions-entry")).toBeFocused();
      await expect(row.getByTestId("mobile-message-actions-entry")).toHaveAttribute("aria-expanded", "false");
      expect((await scrollMetrics(page)).top, "dismissing scrolled nothing").toBe(before);
    }
  });

  test("the + opens only the reaction popup; row and + alternate without stacking", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    for (let round = 0; round < 2; round++) {
      await press(page, own.getByTestId("message-react-button"));
      await expect(page.getByTestId(POPUP)).toBeVisible();
      await expect(sheet(page), "the + never opens actions").toBeHidden();
      await expect(page.getByTestId(POPUP).getByText(/Reply|Edit|Delete/)).toHaveCount(0);
      await press(page, page.getByTestId("mobile-reaction-close"));
      await expect(page.getByTestId(POPUP)).toBeHidden();

      await openMessageActions(page, own);
      await expect(page.getByTestId(POPUP), "a row tap never opens the reaction popup").toBeHidden();
      await press(page, page.getByTestId("mobile-message-actions-close"));
      await expect(sheet(page)).toBeHidden();
    }
    expect(await page.locator("dialog[open]").count(), "nothing left open").toBe(0);
  });

  test("controls inside a row keep their own behaviour and open no actions", async ({ page, context }) => {
    await openRoomWithComposer(page);
    // A link and a mention in raw message HTML, a reply strip, an author
    // name and avatar, a chip and the +.
    await deliver(page, "nested link https://freenet.org/ inside");
    const linkRow = committedRow(page, "nested link");
    const popups: Page[] = [];
    context.on("page", (p) => popups.push(p));
    await linkRow.locator("a[href]").evaluate((a) => a.addEventListener("click", (e) => e.preventDefault()));
    await press(page, linkRow.locator("a[href]"));
    await expectNoSurface(page);
    for (const p of popups) await p.close();

    const quoted = page.getByTestId("message-row").filter({ has: page.getByTestId("reply-strip") }).first();
    await quoted.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await press(page, quoted.getByTestId("reply-strip"));
    await expectNoSurface(page);

    const first = page.getByTestId("message-row").filter({ has: page.getByTestId("message-author-name") }).last();
    await first.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await press(page, first.getByTestId("message-author-name"));
    await expect(page.getByTestId("member-info-modal")).toBeVisible();
    await expect(sheet(page)).toBeHidden();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("member-info-modal")).toBeHidden();
    await press(page, first.getByTestId("message-author-avatar"));
    await expect(page.getByTestId("member-info-modal")).toBeVisible();
    await expect(sheet(page)).toBeHidden();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("member-info-modal")).toBeHidden();

    await callRiverTest(page, "appendMessage", "chip row");
    const chipRow = committedRow(page, "chip row");
    await expect(chipRow).toHaveCount(1);
    await callRiverTest(page, "addReactionsToNewest", ["🎉"]);
    await press(page, chipRow.getByTestId("reaction-chip"));
    await settle(page);
    await expect(sheet(page), "a chip is the chip's").toBeHidden();
    // Its reactor list (a tap) closes from its backdrop, which opens nothing.
    if (await page.getByTestId("reaction-owners").isVisible()) {
      await page.keyboard.press("Escape");
      await expect(page.getByTestId("reaction-owners")).toBeHidden();
    }
    await press(page, chipRow.getByTestId("message-react-button"));
    await expect(page.getByTestId(POPUP)).toBeVisible();
    await expect(sheet(page)).toBeHidden();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId(POPUP)).toBeHidden();

    // The plain part of the same row does open its actions.
    await openMessageActions(page, chipRow);
  });

  test("a press that moved, a text selection and a modified click open nothing", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    const { x, y } = await plainPointOf(own);
    // A press dragged past a tap's slop, released on the row.
    if (await hasTouch(page)) {
      await own.dispatchEvent("pointerdown", { pointerId: 7, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
      await own.dispatchEvent("pointerup", { pointerId: 7, pointerType: "touch", isPrimary: true, clientX: x, clientY: y + 30, bubbles: true });
      await own.dispatchEvent("click", { clientX: x, clientY: y + 30, button: 0, bubbles: true });
    } else {
      await page.mouse.move(x, y);
      await page.mouse.down();
      await page.mouse.move(x, y - 30, { steps: 4 });
      await page.mouse.up();
    }
    await expectNoSurface(page);
    // A press the browser cancelled (it became a scroll).
    await own.dispatchEvent("pointerdown", { pointerId: 8, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("pointercancel", { pointerId: 8, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, bubbles: true });
    await expectNoSurface(page);
    // A second finger joining it: a pinch.
    await own.dispatchEvent("pointerdown", { pointerId: 9, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("pointerdown", { pointerId: 10, pointerType: "touch", isPrimary: false, clientX: x + 40, clientY: y, bubbles: true });
    await own.dispatchEvent("pointerup", { pointerId: 10, pointerType: "touch", isPrimary: false, clientX: x + 40, clientY: y, bubbles: true });
    await own.dispatchEvent("pointerup", { pointerId: 9, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, bubbles: true });
    await expectNoSurface(page);
    // A tap completing a selection in the row.
    await own.getByTestId("message-body").evaluate((el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      const s = window.getSelection()!;
      s.removeAllRanges();
      s.addRange(range);
    });
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, bubbles: true });
    await expectNoSurface(page);
    expect(await page.evaluate(() => window.getSelection()!.toString()), "the selection is left alone").toContain("own first row");
    await page.evaluate(() => window.getSelection()!.removeAllRanges());
    // A Ctrl-click.
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, ctrlKey: true, bubbles: true });
    await expectNoSurface(page);
    // And then a plain tap does.
    await tapRow(page, own);
    await expect(sheet(page)).toBeVisible();
  });

  test("the keyboard has its own entry: shown on focus, Enter opens, Tab stays inside, Escape returns to it", async ({ page, browserName }) => {
    const tab = browserName === "webkit" ? "Alt+Tab" : "Tab";
    const backTab = browserName === "webkit" ? "Alt+Shift+Tab" : "Shift+Tab";
    const { own } = await openRoomWithRows(page);
    const entry = own.getByTestId("mobile-message-actions-entry");
    await expect(entry).toHaveAccessibleName("Message actions");
    await expect(entry).toHaveAttribute("aria-haspopup", "dialog");
    await expect(entry).toHaveAttribute("aria-controls", SHEET);
    await keyboardFocus(page, entry);
    const box = (await entry.boundingBox())!;
    expect(box.width, "keyboard focus shows it").toBeGreaterThan(20);
    await page.keyboard.press("Enter");
    await expect(sheet(page)).toBeVisible();
    await expect(sheet(page)).toHaveAttribute("data-target-row", await rowKey(own));
    const reply = page.getByTestId("mobile-message-action-reply");
    const del = page.getByTestId("mobile-message-action-delete");
    const close = page.getByTestId("mobile-message-actions-close");
    await expect(reply).toBeFocused();
    // Tab order: Close, Reply, Edit, Delete; wrapping both ways.
    await del.focus();
    await page.keyboard.press(tab);
    await expect(close, "Tab from the last control wraps to the first").toBeFocused();
    await page.keyboard.press(backTab);
    await expect(del, "Shift+Tab from the first wraps to the last").toBeFocused();
    expect(await page.locator("dialog[open]").count()).toBe(1);
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await expect(entry, "focus returns to the entry").toBeFocused();
    // Space opens it once too.
    await page.keyboard.press("Space");
    await expect(sheet(page)).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
  });

  test("a backdrop tap closes it and reaches nothing underneath", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await openMessageActions(page, own);
    // Over the header's rooms button: the tap must not open the rooms panel.
    const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true }).first();
    const hb = (await hamburger.boundingBox())!;
    const point = { x: hb.x + hb.width / 2, y: hb.y + hb.height / 2 };
    const hit = await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid") ?? null, point);
    expect(hit, "premise: the backdrop covers the header").toBe("mobile-message-actions-backdrop");
    if (await hasTouch(page)) await page.touchscreen.tap(point.x, point.y);
    else await page.mouse.click(point.x, point.y);
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    await expect(page.locator("#chat-scroll-container"), "the chat is still the panel shown").toBeVisible();
    await expect(page.getByTestId("room-list").filter({ visible: true }), "the rooms panel did not open").toHaveCount(0);
  });

  test("Reply, Edit and Delete act on the exact message, closing first", async ({ page }) => {
    const { own, continuation } = await openRoomWithRows(page);
    const composer = page.getByTestId("message-composer");

    await openMessageActions(page, continuation);
    await press(page, page.getByTestId("mobile-message-action-reply"));
    await expect(sheet(page)).toBeHidden();
    await expect(composer).toContainText("Replying to");
    await expect(composer).toContainText("own continuation row");
    await expect(page.getByTestId("message-input"), "the reply hands focus to the composer").toBeFocused();
    await page.getByTitle("Cancel reply").click();

    const ownKey = await rowKey(own);
    const ownRow = page.locator(`[id="msg-${ownKey}"]`);
    await openMessageActions(page, own);
    await press(page, page.getByTestId("mobile-message-action-edit"));
    await expect(sheet(page)).toBeHidden();
    const editArea = page.locator(`textarea[id="edit-msg-${ownKey}"]`);
    await expect(editArea, "the editor opens on that row").toBeVisible();
    await expect(editArea).toHaveValue("own first row");
    await expect(editArea).toBeFocused();
    await expect(ownRow.getByTestId("message-react-button"), "no + on a row being edited").toHaveCount(0);
    await expect(ownRow.getByTestId("mobile-message-actions-entry")).toHaveCount(0);
    // A tap on the row being edited opens nothing.
    await ownRow.locator(".msg-gutter").first().dispatchEvent("click", { button: 0, bubbles: true });
    await expectNoSurface(page);
    await press(page, ownRow.getByTestId("edit-cancel-button"));
    await expect(editArea).toHaveCount(0);

    await openMessageActions(page, continuation);
    await press(page, page.getByTestId("mobile-message-action-delete"));
    const heading = page.getByRole("heading", { name: /Delete Message/ });
    await expect(heading).toBeVisible();
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(heading).toHaveCount(0);
    await expect(continuation).toHaveCount(1);
    await openMessageActions(page, continuation);
    await press(page, page.getByTestId("mobile-message-action-delete"));
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await expect(continuation).toHaveCount(0);
    await expect(own, "only that message").toHaveCount(1);
  });

  test("an unsigned send has no actions; once signed it has its own", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "holdMessageSigning");
    await submitMessage(page, "still signing");
    const pending = page.getByTestId("pending-message").filter({ hasText: "still signing" });
    await expect(pending).toBeVisible();
    await page.getByTestId("message-input").blur();
    await press(page, pending.getByTestId("pending-message-text"));
    await expectNoSurface(page);
    await expect(pending.getByTestId("mobile-message-actions-entry")).toHaveCount(0);
    await callRiverTest(page, "releaseMessageSigning");
    const signed = committedRow(page, "still signing");
    await expect(signed).toHaveCount(1);
    await openMessageActions(page, signed);
    await expect(page.getByTestId("mobile-message-action-edit")).toBeVisible();
  });

  test("its message leaving closes it for good; a remount keeps it; a room switch and 48rem close it", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    // Remount: removing a group's first message re-keys the group.
    await callRiverTest(page, "deliverTimedMessages", 0, [
      { text: "sheet lead", sentSecs: 1, receivedSecs: 0 },
      { text: "sheet survivor", sentSecs: 2, receivedSecs: 0 },
    ]);
    const survivor = committedRow(page, "sheet survivor");
    await expect(survivor).toHaveCount(1);
    await openMessageActions(page, survivor);
    await survivor.evaluate((el) => ((window as unknown as { __old: Element }).__old = el));
    await callRiverTest(page, "removeMessages", "sheet lead");
    await expect(committedRow(page, "sheet lead")).toHaveCount(0);
    await settle(page);
    expect(await survivor.evaluate((el) => el === (window as unknown as { __old: Element }).__old), "premise: remounted").toBe(false);
    await expect(sheet(page), "a remount keeps it").toBeVisible();
    await page.keyboard.press("Escape");
    await expect(survivor.getByTestId("mobile-message-actions-entry"), "focus goes to the new entry").toBeFocused();

    // Deleted while open (optimistically, signature held), then rolled back.
    const own = await submitMessage(page, "sheet doomed");
    await expect(own).toHaveCount(1);
    await page.getByTestId("message-input").blur();
    await callRiverTest(page, "holdMessageSigning");
    await openMessageActions(page, own);
    await press(page, page.getByTestId("mobile-message-action-delete"));
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await expect(own).toHaveCount(0);
    await openMessageActions(page, committedRow(page, "sheet survivor"));
    await callRiverTest(page, "removeMessages", "sheet survivor");
    await expect(sheet(page), "its message left: it closes").toBeHidden();
    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(own, "rolled back").toHaveCount(1);
    await settle(page);
    await expect(sheet(page), "a rollback never reopens it").toBeHidden();

    // A room switch.
    await openMessageActions(page, own);
    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await expect(sheet(page)).toBeHidden();
    await callRiverTest(page, "switchRoom", "Your Private Room");
    await expect(own).toHaveCount(1);
    await settle(page);
    await expect(sheet(page)).toBeHidden();

    // Crossing to 48rem, and back.
    await openMessageActions(page, own);
    await page.setViewportSize({ width: 768, height: 760 });
    await expect(sheet(page)).toBeHidden();
    await page.setViewportSize({ width: 390, height: 760 });
    await settle(page);
    await expect(sheet(page), "never reopened by resizing back").toBeHidden();
  });

  test("an action queued for an earlier opening does nothing", { tag: "@chromium-only" }, async ({ page }) => {
    const { own, received } = await openRoomWithRows(page);
    await openMessageActions(page, own);
    // In one task: Delete pressed, then the sheet dismissed and opened again
    // on another message before the Delete runs.
    await page.evaluate(() => {
      const del = document.querySelector<HTMLElement>('[data-testid="mobile-message-action-delete"]')!;
      del.click();
    });
    await settle(page);
    // The delete went to the confirmation, closing the sheet first.
    await expect(page.getByRole("heading", { name: /Delete Message/ })).toBeVisible();
    await page.getByRole("button", { name: "Cancel", exact: true }).click();

    await openMessageActions(page, own);
    const receivedKey = await rowKey(received);
    await page.evaluate((receivedKey) => {
      const reply = document.querySelector<HTMLElement>('[data-testid="mobile-message-action-reply"]')!;
      const close = document.querySelector<HTMLElement>('[data-testid="mobile-message-actions-close"]')!;
      const entry = document.querySelector<HTMLElement>(`[id="msg-${receivedKey}"] [data-testid="mobile-message-actions-entry"]`)!;
      close.click();
      entry.click();
      reply.click();
    }, receivedKey);
    await settle(page);
    await expect(page.getByTestId("message-composer"), "the stale Reply did nothing").not.toContainText("Replying to");
    await expect(sheet(page), "the newer opening is open").toBeVisible();
    await expect(sheet(page)).toHaveAttribute("data-target-row", receivedKey);
  });

  test("while open the background holds still, arrivals follow, and a reload waits; closing lets go", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await openMessageActions(page, own);
    expect(await callRiverTest(page, "userIsBusy"), "busy while open").toBe(true);
    if (!(await hasTouch(page))) {
      const view = (await page.locator("#chat-scroll-container").boundingBox())!;
      const before = (await scrollMetrics(page)).top;
      expect(before, "premise: the history has room to scroll up").toBeGreaterThan(300);
      await page.mouse.move(view.x + 30, view.y + 30);
      await page.mouse.wheel(0, -300);
      await settle(page);
      expect((await scrollMetrics(page)).top, "the history did not move").toBe(before);
      // A zoom is let through: not cancelled.
      const cancelled = await page.evaluate(({ x, y }) => {
        const target = document.elementFromPoint(x, y)!;
        const e = new WheelEvent("wheel", { deltaY: -100, ctrlKey: true, bubbles: true, cancelable: true, clientX: x, clientY: y });
        target.dispatchEvent(e);
        return e.defaultPrevented;
      }, { x: view.x + 30, y: view.y + 30 });
      expect(cancelled, "Ctrl+wheel is a zoom").toBe(false);
    }
    // An arrival is still followed by a reader at the end.
    await callRiverTest(page, "appendMessage", "arrives under the sheet");
    await expect(committedRow(page, "arrives under the sheet")).toBeInViewport();
    await expect(sheet(page)).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "closed: not busy" }).toBe(false);
    if (!(await hasTouch(page))) {
      const view = (await page.locator("#chat-scroll-container").boundingBox())!;
      const before = (await scrollMetrics(page)).top;
      await page.mouse.move(view.x + 30, view.y + 30);
      await page.mouse.wheel(0, -300);
      await expect.poll(async () => (await scrollMetrics(page)).top, { message: "closed: the wheel scrolls again" }).toBeLessThan(before);
    }
  });


  test("one-finger touch is held outside the panel, not inside it or for a pinch; every close path lets go", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    const closes: [string, () => Promise<void>][] = [
      ["Close", () => press(page, page.getByTestId("mobile-message-actions-close"))],
      ["Escape", () => page.keyboard.press("Escape")],
      ["backdrop", async () => {
        const p = await historyPoint(page);
        if (await hasTouch(page)) await page.touchscreen.tap(p.x, p.y);
        else await page.mouse.click(p.x, p.y);
      }],
      ["48rem", async () => {
        await page.setViewportSize({ width: 768, height: 760 });
        await expect(sheet(page)).toBeHidden();
        await page.setViewportSize({ width: 390, height: 760 });
      }],
    ];
    for (const [how, close] of closes) {
      await openMessageActions(page, own);
      const h = await historyPoint(page);
      expect(await touchmovePrevented(page, h.x, h.y), `${how}: a finger over the history is held`).toBe(true);
      expect(await touchmovePrevented(page, h.x, h.y, 2), `${how}: a pinch is not`).toBe(false);
      const inside = await panelPoint(page, SHEET);
      expect(await touchmovePrevented(page, inside.x, inside.y), `${how}: the panel scrolls`).toBe(false);
      await close();
      await expect(sheet(page)).toBeHidden();
      await settle(page);
      const after = await historyPoint(page);
      expect(await page.evaluate(({ x, y }) => !!document.elementFromPoint(x, y)?.closest("#chat-scroll-container"), after), "premise: the history is under the point").toBe(true);
      expect(await touchmovePrevented(page, after.x, after.y), `closed by ${how}: let go`).toBe(false);
    }
    // Its message leaving.
    await callRiverTest(page, "appendMessage", "touch leaving row");
    const leaving = committedRow(page, "touch leaving row");
    await expect(leaving).toHaveCount(1);
    await openMessageActions(page, leaving);
    await callRiverTest(page, "removeMessages", "touch leaving row");
    await expect(sheet(page)).toBeHidden();
    const after = await historyPoint(page);
    expect(await touchmovePrevented(page, after.x, after.y), "closed by its message leaving: let go").toBe(false);
  });

  test("an arrival carrying its row out of view keeps it open, usable and holding", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await openMessageActions(page, own);
    const tall = Array.from({ length: 40 }, (_, i) => `tall arrival ${i}`).join("\n\n");
    await callRiverTest(page, "appendMessage", tall);
    await expect(page.getByTestId("message-row").filter({ hasText: "tall arrival 39" })).toHaveCount(1);
    await settle(page);
    const ownBox = await own.evaluate((el) => {
      const r = el.getBoundingClientRect();
      const v = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
      return { above: r.bottom <= v.top, connected: el.isConnected };
    });
    expect(ownBox, "premise: still rendered, out of the history's view").toEqual({ above: true, connected: true });
    await expect(sheet(page), "still open").toBeVisible();
    const h = await historyPoint(page);
    expect(await touchmovePrevented(page, h.x, h.y), "still holding the background").toBe(true);
    await press(page, page.getByTestId("mobile-message-action-reply"));
    await expect(page.getByTestId("message-composer")).toContainText("own first row");
  });

  test("leaving the chat panel or a re-key of the room closes it, and a queued action then does nothing", { tag: "@chromium-only" }, async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await openMessageActions(page, own);
    await callRiverTest(page, "setMobileView", "members");
    await expect(sheet(page)).toBeHidden();
    await callRiverTest(page, "setMobileView", "chat");
    await settle(page);
    await expect(sheet(page), "coming back reopens nothing").toBeHidden();

    await openMessageActions(page, own);
    // The re-key and a Reply pressed in the same task, the re-key first.
    await page.evaluate(() => {
      (window as unknown as { __riverTest: { rekeyCurrentRoom(): void } }).__riverTest.rekeyCurrentRoom();
      document.querySelector<HTMLElement>('[data-testid="mobile-message-action-reply"]')!.click();
    });
    await settle(page);
    await expect(sheet(page)).toBeHidden();
    await expect(page.getByTestId("message-composer"), "the Reply queued across the re-key did nothing").not.toContainText("Replying to");
    // In the new generation a fresh opening works.
    await openMessageActions(page, own);
  });

  test("join notices and DM rows open nothing", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "appendJoinEvents", 2);
    const notice = page.getByTestId("join-notice").last();
    await expect(notice).toBeVisible();
    const nb = (await notice.boundingBox())!;
    if (await hasTouch(page)) await page.touchscreen.tap(nb.x + nb.width - 4, nb.y + nb.height / 2);
    else await page.mouse.click(nb.x + nb.width - 4, nb.y + nb.height / 2);
    await expectNoSurface(page);
    await expect(page.getByTestId("date-separator").first()).toBeAttached();

    await callRiverTest(page, "appendDmsForPeer", 0, 3);
    await openThread(page, DM_PEER);
    const dmRow = page.getByTestId("dm-row").last();
    await expect(dmRow).toBeVisible();
    await press(page, dmRow);
    await expectNoSurface(page);
    await expect(dmRow.getByTestId("mobile-message-actions-entry")).toHaveCount(0);
  });

  test("the sheet offers exactly what the desktop footer does, row by row", async ({ page }) => {
    // Self owns this room: owning it grants no Edit or Delete on another
    // author's message, here or in the footer.
    const { received, own } = await openRoomWithRows(page);
    const sheetActions: Record<string, string[]> = {};
    for (const [name, row] of [["received", received], ["own", own]] as const) {
      await openMessageActions(page, row);
      sheetActions[name] = await sheet(page).locator(".mobile-sheet-actions button").evaluateAll((bs) =>
        bs.map((b) => b.getAttribute("data-testid")!.replace("mobile-message-action-", "")),
      );
      await page.keyboard.press("Escape");
      await expect(sheet(page)).toBeHidden();
    }
    await page.setViewportSize({ width: 900, height: 760 });
    for (const [name, row] of [["received", received], ["own", own]] as const) {
      const footer = await row.getByTestId("message-action-cluster").locator("button").evaluateAll((bs) =>
        bs.map((b) => b.getAttribute("data-testid")!.replace(/^message-|-button$/g, "")),
      );
      expect(sheetActions[name], `${name}: sheet and footer agree`).toEqual(footer);
    }
    expect(sheetActions.received).toEqual(["reply"]);
  });

  test("a double- or triple-click selects text in a narrow mouse window and opens nothing", async ({ page }) => {
    test.skip(await hasTouch(page), "a mouse's multi-click; a finger opens at once");
    const { own } = await openRoomWithRows(page);
    const word = own.getByTestId("message-body").locator("p").first();
    await word.dblclick({ position: { x: 6, y: 6 } });
    await expectNoSurface(page);
    expect((await page.evaluate(() => window.getSelection()!.toString())).trim(), "a word is selected").not.toBe("");
    await word.click({ position: { x: 6, y: 6 }, clickCount: 3 });
    await expectNoSurface(page);
    expect(await page.evaluate(() => window.getSelection()!.toString()), "the line is selected").toContain("own first row");
    await page.evaluate(() => window.getSelection()!.removeAllRanges());
    // A single click opens it, once the double-click window has passed.
    await tapRow(page, own);
    await expect(sheet(page)).toBeVisible();
  });

  test("a contact whose release never reached a row does not make later taps a pinch", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    const { x, y } = await plainPointOf(own);
    // A finger down on a row that was replaced before it lifted: no pointerup.
    await own.dispatchEvent("pointerdown", { pointerId: 30, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    // The next finger is the only one down.
    await own.dispatchEvent("pointerdown", { pointerId: 31, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("pointerup", { pointerId: 31, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, bubbles: true });
    await expect(sheet(page)).toBeVisible();
  });

  test("opening and closing it renders no message group", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await settle(page);
    await callRiverTest(page, "takeMessageGroupRenders");
    await openMessageActions(page, own);
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    expect(await callRiverTest(page, "takeMessageGroupRenders")).toBe(0);
  });
});

/// The open panel's geometry against the viewport.
function panelGeometry(page: Page, id: string) {
  return page.evaluate((id) => {
    const dialog = document.getElementById(id)!;
    const panel = dialog.querySelector(".mobile-sheet-panel") as HTMLElement;
    const r = panel.getBoundingClientRect();
    const buttons = Array.from(panel.querySelectorAll("button")).map((b) => {
      const br = b.getBoundingClientRect();
      return { id: b.getAttribute("data-testid") ?? b.textContent, w: br.width, h: br.height, left: br.left, right: br.right };
    });
    return {
      vw: document.documentElement.clientWidth,
      vh: window.innerHeight,
      left: r.left,
      right: r.right,
      top: r.top,
      bottom: r.bottom,
      scrollable: panel.scrollHeight > panel.clientHeight,
      overflowX: document.documentElement.scrollWidth > document.documentElement.clientWidth,
      buttons,
      background: getComputedStyle(panel).backgroundColor,
    };
  }, id);
}

test.describe("The actions sheet at 320px", () => {
  test.use({ viewport: { width: 320, height: 568 } });

  test("fits, bottom-aligned, with every target at least 44px; light and dark", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    for (const scheme of ["light", "dark"] as const) {
      await page.emulateMedia({ colorScheme: scheme });
      await openMessageActions(page, own);
      const g = await panelGeometry(page, SHEET);
      expect(g.overflowX, "no page overflow").toBe(false);
      expect(g.left).toBeGreaterThanOrEqual(0);
      expect(g.right).toBeLessThanOrEqual(g.vw);
      expect(Math.abs(g.bottom - g.vh), "on the bottom edge").toBeLessThan(2);
      expect(g.top).toBeGreaterThanOrEqual(0);
      for (const b of g.buttons) {
        expect(b.h, `${b.id} height`).toBeGreaterThanOrEqual(44);
        expect(b.w, `${b.id} width`).toBeGreaterThanOrEqual(44);
        expect(b.right, `${b.id} inside`).toBeLessThanOrEqual(g.vw);
      }
      const panelToken = await page.evaluate(() => {
        const probe = document.createElement("div");
        probe.style.backgroundColor = "var(--color-panel)";
        document.body.appendChild(probe);
        const c = getComputedStyle(probe).backgroundColor;
        probe.remove();
        return c;
      });
      expect(g.background, `${scheme}: the panel is the theme's panel colour`).toBe(panelToken);
      await page.keyboard.press("Escape");
      await expect(sheet(page)).toBeHidden();
    }
  });
});

test.describe("At 48rem and wider", () => {
  test.use({ viewport: { width: 768, height: 760 } });

  test("the footer and the anchored picker are back, and a row tap opens nothing", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    expect(await narrowLayout(page), "premise: 48rem").toBe(false);
    const display = await own.evaluate((el) => ({
      cluster: getComputedStyle(el.querySelector('[data-testid="message-action-cluster"]')!).display,
      entry: getComputedStyle(el.querySelector('[data-testid="mobile-message-actions-entry"]')!).display,
    }));
    expect(display).toEqual({ cluster: "flex", entry: "none" });
    expect(await tabbableIds(page)).not.toContain("mobile-message-actions-entry");
    await tapRow(page, own);
    await expectNoSurface(page);
    if (!(await coarsePointer(page))) await own.hover({ position: { x: 4, y: 4 } });
    const react = own.getByTestId("message-react-button");
    await expect(react).toHaveAttribute("aria-controls", "reaction-picker");
    await press(page, react);
    await expect(page.getByTestId("emoji-picker")).toBeVisible();
    await expect(page.getByTestId(POPUP)).toBeHidden();
  });
});

test.describe("The narrow query", () => {
  // No engine here lays a page out at a fractional width (a 767.5px frame is
  // snapped to 768), so the property is checked as what it is: the rules and
  // the press routing use `not all and (min-width: 48rem)`, the exact
  // complement of Tailwind's `md`, which leaves no width to neither layout.
  test("is the exact complement of md, in the stylesheet and at the press", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    const rules = await page.evaluate(() => {
      const out: string[] = [];
      for (const sheet of Array.from(document.styleSheets)) {
        let list: CSSRuleList;
        try {
          list = sheet.cssRules;
        } catch {
          continue;
        }
        for (const rule of Array.from(list)) {
          if (rule instanceof CSSMediaRule && /msg-action-cluster|mobile-actions-entry/.test(rule.cssText)) out.push(rule.conditionText);
        }
      }
      return out;
    });
    expect(rules.length).toBeGreaterThan(0);
    // Engines serialize the range in either syntax.
    for (const condition of rules) expect(condition).toMatch(/^not all and \((min-width: 48rem|width >= 48rem)\)$/);
    for (const width of [767, 768]) {
      await page.setViewportSize({ width, height: 760 });
      const state = await page.evaluate(() => ({
        md: window.matchMedia("(min-width: 48rem)").matches,
        narrow: window.matchMedia("not all and (min-width: 48rem)").matches,
        cluster: getComputedStyle(document.querySelector('[data-testid="message-action-cluster"]')!).display,
      }));
      expect(state.narrow, `${width}px`).toBe(!state.md);
      expect(state.cluster, `${width}px`).toBe(state.narrow ? "none" : "flex");
    }
  });
});

test.describe("Inside the gateway's sandboxed frame", () => {
  test("a row tap opens the sheet and + the popup with its field focused", async ({ page }) => {
    // The frame is the variable here, not the device: the desktop engines
    // cover it (a frame inside an emulated phone viewport did not take the
    // room list's clicks).
    test.skip(test.info().project.name.startsWith("mobile"), "covered by the desktop engines");
    const app = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:8082";
    await page.setViewportSize({ width: 600, height: 760 });
    // The gateway's sandbox, plus same-origin so the local build can load.
    await page.setContent(`<!DOCTYPE html><html><body style="margin:0;height:100vh">
      <iframe sandbox="allow-scripts allow-forms allow-popups allow-same-origin" src="${app}"
        style="width:100%;height:100%;border:0;display:block"></iframe></body></html>`);
    const frame = page.frameLocator("iframe");
    await frame.locator(".app-root").waitFor({ timeout: 30_000 });
    await frame.getByTestId("hamburger-rooms-button").filter({ visible: true }).first().click();
    await frame.getByTestId("room-list").getByRole("button", { name: "Your Private Room" }).click();
    const row = frame.getByTestId("message-row").last();
    await expect(row).toBeVisible();
    expect(await row.evaluate((el) => getComputedStyle(el.querySelector('[data-testid="message-action-cluster"]')!).display)).toBe("none");
    const content = row.getByTestId("message-content");
    const cb = (await content.boundingBox())!;
    await content.click({ position: { x: cb.width - 3, y: Math.min(8, cb.height / 2) } });
    const s = frame.getByTestId(SHEET);
    await expect(s).toBeVisible();
    await expect(s).toHaveAttribute("data-target-row", (await row.getAttribute("id"))!.replace(/^msg-/, ""));
    const panel = await s.locator(".mobile-sheet-panel").boundingBox();
    expect(panel!.y + panel!.height, "bottom-aligned in the frame").toBeGreaterThan(740);
    await frame.getByTestId("mobile-message-actions-close").click();
    await expect(s).toBeHidden();
    await row.getByTestId("message-react-button").click();
    await expect(frame.getByTestId(POPUP)).toBeVisible();
    await expect(frame.getByTestId("mobile-reaction-input")).toBeFocused();
  });
});
