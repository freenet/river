import { test, expect, Locator, Page } from "@playwright/test";
import { coarsePointer, nextFrames, openInlinePicker, openOwnMessageEdit, openRoomWithComposer, reactTo, resolveColor, selectRoom, touchOnly, waitForApp } from "./example-room";
import { deliver, deliverOffscreen, readerLeavesTheEnd, readerScrollsWithoutGesture, scrollMetrics } from "./history-geometry";
import { callRiverTest } from "./river-test";

// Grouped chat rows: an avatar gutter, then one content column for every row.
// A group shows its avatar and header once, on its first row. On a hover
// pointer the action bar floats over a row's top right, so no gutter is kept
// for it; touch keeps a kebab in a narrow right gutter.

/// The history item (a group of one author's rows) holding `text`.
function groupWith(page: Page, text: string): Locator {
  return page.locator("[data-item-key]").filter({ hasText: text });
}

/// A received pair (one fixed test identity, inserted before the newest
/// message) and an own pair (two sends from the composer). Example data picks
/// its authors at random, so neither pair is left to chance.
async function openRoomWithPairs(page: Page): Promise<{ received: Locator; own: Locator }> {
  await openRoomWithComposer(page);
  for (const text of ["received pair one", "received pair two"]) {
    await callRiverTest(page, "insertMessageBeforeLast", text);
  }
  const input = page.getByTestId("message-input");
  for (const text of ["own pair one", "own pair two"]) {
    await input.fill(text);
    await input.press("Enter");
    await expect(page.getByTestId("conversation-history")).toContainText(text);
  }
  const received = groupWith(page, "received pair one");
  const own = groupWith(page, "own pair one");
  await expect(received).toContainText("received pair two");
  await expect(own).toContainText("own pair two");
  return { received, own };
}

const width = (b: { left: number; right: number }) => b.right - b.left;

function rowGeometry(group: Locator) {
  return group.evaluate((el) => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    return Array.from(el.querySelectorAll('[data-testid="message-row"]')).map((row) => ({
      self: row.getAttribute("data-message-self"),
      rowLeft: row.getBoundingClientRect().left,
      contentLeft: row.querySelector('[data-testid="message-content"]')!.getBoundingClientRect().left,
      avatars: Array.from(row.querySelectorAll('[data-testid="message-author-avatar"]')).map(box),
      headers: Array.from(row.querySelectorAll('[data-testid="message-group-header"]')).map(box),
      names: Array.from(row.querySelectorAll('[data-testid="message-author-name"]')).map(box),
      body: box(row.querySelector('[data-testid="message-body"]')!),
    }));
  });
}

test.describe("Message rows", () => {
  test("own and received groups share one row layout, with one avatar and header per group", async ({ page }) => {
    const { received, own } = await openRoomWithPairs(page);
    const groups = { received: await rowGeometry(received), own: await rowGeometry(own) };

    const bodyLefts: number[] = [];
    for (const [kind, rows] of Object.entries(groups)) {
      expect(rows, `${kind}: a two-message group`).toHaveLength(2);
      expect(rows.map((r) => r.self)).toEqual(Array(2).fill(kind === "own" ? "true" : "false"));
      // One avatar and one header, in the first row.
      expect(rows.map((r) => r.avatars.length), `${kind}: avatars per row`).toEqual([1, 0]);
      expect(rows.map((r) => r.headers.length), `${kind}: headers per row`).toEqual([1, 0]);

      const [first, second] = rows;
      // The content column starts past the avatar gutter, and the avatar sits in it.
      expect(first.contentLeft - first.rowLeft, `${kind}: the gutter`).toBeGreaterThanOrEqual(48);
      expect(first.avatars[0].left, `${kind}: avatar inside the row`).toBeGreaterThanOrEqual(first.rowLeft - 0.5);
      expect(first.avatars[0].right, `${kind}: avatar in the gutter`).toBeLessThanOrEqual(first.contentLeft + 0.5);
      // The header and every body start at the content edge.
      expect(first.headers[0].left, `${kind}: header at the content edge`).toBeCloseTo(first.contentLeft, 0);
      expect(first.body.left, `${kind}: body at the content edge`).toBeCloseTo(first.contentLeft, 0);
      expect(second.body.left, `${kind}: continuation aligned`).toBeCloseTo(first.body.left, 0);

      bodyLefts.push(...rows.map((r) => r.body.left));
    }
    // Same left edge for both authors: no right-aligned own group.
    expect(Math.max(...bodyLefts) - Math.min(...bodyLefts)).toBeLessThanOrEqual(0.5);
  });

  test("the author name is a button, by click and by Enter, and the avatar opens the same info", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "insertMessageBeforeLast", "name opens info");
    const group = groupWith(page, "name opens info");
    const modal = page.getByTestId("member-info-modal");

    await group.getByRole("button", { name: "Test Sender Insert" }).click();
    await expect(modal).toContainText("Test Sender Insert");
    await page.keyboard.press("Escape");
    await expect(modal).toHaveCount(0);

    // Keyboard: focus and Enter, the native button's own activation.
    await group.getByTestId("message-author-name").focus();
    await page.keyboard.press("Enter");
    await expect(modal).toContainText("Test Sender Insert");
    await page.keyboard.press("Escape");
    await expect(modal).toHaveCount(0);

    // The avatar is a pointer shortcut only: hidden from assistive tech and no tab stop.
    const avatar = group.getByTestId("message-author-avatar");
    await expect(avatar).toHaveAttribute("aria-hidden", "true");
    expect(await avatar.evaluate((el) => (el as HTMLElement).tabIndex), "the avatar is not a tab stop").toBe(-1);
    await avatar.click();
    await expect(modal).toContainText("Test Sender Insert");
  });

  // freenet/river#791 review: without bubbles, a body could imitate a group
  // header. A body's first line sits beside the gutter of a continuation row,
  // which never holds an avatar, and it can never reach into the gutter.
  test("a body that opens like a header has no avatar and cannot reach the gutter", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "insertMessageBeforeLast", "the real header");
    await callRiverTest(page, "insertMessageBeforeLast", "**Test Sender Insert** 🛡 forged header");
    const group = groupWith(page, "the real header");
    await expect(group).toContainText("forged header");
    const rows = group.getByTestId("message-row");
    const forged = rows.filter({ hasText: "forged header" });
    await expect(forged.locator("strong"), "premise: the body renders bold").toHaveCount(1);

    const m = await group.evaluate((el) => {
      const rows = Array.from(el.querySelectorAll('[data-testid="message-row"]'));
      const real = rows[0];
      const fake = rows.find((r) => r.textContent!.includes("forged header"))!;
      const box = (e: Element | null) => e && e.getBoundingClientRect();
      return {
        realAvatar: box(real.querySelector('[data-testid="message-author-avatar"]')),
        realName: box(real.querySelector('[data-testid="message-author-name"]'))!,
        fakeAvatars: fake.querySelectorAll('[data-testid="message-author-avatar"]').length,
        fakeHeaders: fake.querySelectorAll('[data-testid="message-group-header"]').length,
        fakeStrong: box(fake.querySelector("strong"))!,
        fakeGutterRight: box(fake.querySelector(".msg-gutter"))!.right,
      };
    });
    expect(m.fakeHeaders, "the forged line is no header").toBe(0);
    expect(m.fakeAvatars, "the forged line carries no avatar").toBe(0);
    expect(m.realAvatar, "the real header carries one").not.toBeNull();
    // The real name has its avatar beside it, in the gutter, on its own line.
    expect(m.realAvatar!.right).toBeLessThanOrEqual(m.realName.left);
    expect(m.realAvatar!.bottom).toBeGreaterThan(m.realName.top);
    // The forged bold starts past the gutter, so it can never sit where an avatar does.
    expect(m.fakeStrong.left).toBeGreaterThanOrEqual(m.fakeGutterRight - 0.5);
  });

  test.describe("at a desktop width", () => {
    // Wide enough that the column exceeds the measure with both side panels
    // and the touch gutter (at 1280 a touch project's column is just under it).
    test.use({ viewport: { width: 1440, height: 900 } });

    // #791 review: nothing caught a width cap (65ch, max-w-prose, max-w-4xl)
    // coming back. Rows span the conversation column (the panel, up to what a
    // row needs) and a long body fills the 76ch measure.
    test("rows span the column and a long body fills its 76ch measure", async ({ page }) => {
      await openRoomWithComposer(page);
      const long = "A long line that keeps going ".repeat(12) + "until the end";
      await deliver(page, long);
      const row = page.getByTestId("message-row").filter({ hasText: "until the end" });
      const m = await columnMetrics(row);
      expect(m.rowWidth, "the row spans the column").toBeCloseTo(m.columnWidth, 0);
      expect(m.bodyWidth, "the body fills the 76ch measure").toBeCloseTo(m.measure, 0);
      expect(m.columnWidth, "the column is the panel, up to what a row needs").toBeCloseTo(
        Math.min(m.scrollerInner, m.rowNeeds),
        0,
      );
    });
  });
});

/// Where a row sits in the history: the conversation column (`#chat-content`,
/// `.conversation-column`) inside the scroller's client area, and what a row
/// needs (its gutter, the body's measure and its right padding).
async function columnMetrics(row: Locator) {
  return row.evaluate((el) => {
    const scroller = document.getElementById("chat-scroll-container")!;
    const s = scroller.getBoundingClientRect();
    const column = document.getElementById("chat-content")!.getBoundingClientRect();
    const body = el.querySelector('[data-testid="message-body"]')!;
    const probe = document.createElement("div");
    probe.style.width = "76ch";
    body.appendChild(probe);
    const measure = probe.getBoundingClientRect().width;
    probe.remove();
    const cs = getComputedStyle(el);
    const gutter = parseFloat(cs.gridTemplateColumns);
    return {
      scrollerInner: scroller.clientWidth,
      columnLeft: column.left - s.left,
      columnRight: s.left + scroller.clientWidth - column.right,
      columnWidth: column.width,
      rowLeft: el.getBoundingClientRect().left - s.left,
      rowWidth: el.getBoundingClientRect().width,
      bodyWidth: body.getBoundingClientRect().width,
      bodyLeft: body.getBoundingClientRect().left - s.left,
      measure,
      rowNeeds: gutter + measure + parseFloat(cs.paddingRight),
    };
  });
}

// The design critique's first complaint was a narrow column centred in a
// broad empty panel; its fix (#823) let rows span the panel, and on a wide
// window that left the conversation hugging the left with an empty band on
// the right. The conversation COLUMN is now centred once the panel is wider
// than a row needs: equal gaps either side, text left-aligned at its measure.
// The scroller stays panel-wide, so its scrollbar is at the panel's edge.
test.describe("at 1920px", () => {
  test.use({ viewport: { width: 1920, height: 1080 } });

  test("the conversation column is centred, and text keeps its measure", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "A wide-window line that keeps going ".repeat(12) + "to the wide end");
    const row = page.getByTestId("message-row").filter({ hasText: "to the wide end" });
    const m = await columnMetrics(row);
    expect(m.scrollerInner, "premise: the panel is far wider than a row needs").toBeGreaterThan(m.rowNeeds + 200);
    expect(m.columnWidth, "the column is what a row needs").toBeCloseTo(m.rowNeeds, 0);
    expect(Math.abs(m.columnLeft - m.columnRight), "equal side gaps").toBeLessThanOrEqual(1);
    expect(m.rowLeft, "the row starts at the column's edge").toBeCloseTo(m.columnLeft, 0);
    expect(m.bodyWidth, "the text keeps its 76ch measure").toBeCloseTo(m.measure, 0);
    // The scroller itself spans the panel (its scrollbar at the panel's edge).
    const panel = await page.locator("#chat-scroll-container").evaluate((el) => ({
      scroller: el.getBoundingClientRect().width,
      parent: el.parentElement!.getBoundingClientRect().width,
    }));
    expect(panel.scroller).toBeCloseTo(panel.parent, 0);

    // The composer sits in the same column: its box starts at the column's
    // left plus the composer's padding, as it starts at the panel's otherwise.
    // Centred in the panel, where the history centres in the scroller's
    // client area: they differ by half a classic scrollbar, if any.
    const composer = await page.getByTestId("message-composer").evaluate((el) => {
      const sc = document.getElementById("chat-scroll-container")!;
      const s = sc.getBoundingClientRect();
      const inner = el.firstElementChild as HTMLElement;
      const r = inner.getBoundingClientRect();
      return { left: r.left - s.left, width: r.width, halfScrollbar: (sc.offsetWidth - sc.clientWidth) / 2 };
    });
    expect(Math.abs(composer.left - m.columnLeft - composer.halfScrollbar), "the composer's column lines up with the history's").toBeLessThanOrEqual(1);
    expect(composer.width).toBeCloseTo(m.columnWidth, 0);

    // A date separator stays within the column, its label over the text.
    await callRiverTest(page, "appendJoinEvents", 1);
    const sep = page.getByTestId("date-separator").last();
    await expect(sep).toBeAttached();
    const d = await sep.evaluate((el) => {
      const s = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
      const r = el.getBoundingClientRect();
      const range = document.createRange();
      range.selectNodeContents(el);
      const label = range.getBoundingClientRect();
      return { left: r.left - s.left, right: r.right - s.left, labelCentre: (label.left + label.right) / 2 - s.left };
    });
    expect(d.left, "the separator is inside the column").toBeGreaterThanOrEqual(m.columnLeft);
    expect(d.right, "the separator ends near the text's measure").toBeLessThanOrEqual(m.columnLeft + 60 + m.measure + 2);
    expect(d.labelCentre, "its label sits over the text column").toBeLessThan(m.columnLeft + 60 + m.measure);
  });

  // Horizontal only: every line in a row is already bounded by the measure,
  // so centring the column changes no row's height, and so nothing the
  // history's scroll corrections measure (history-scrolling.md).
  test("centring changes no row's height", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "A wide-window line that keeps going ".repeat(12) + "to the wide end");
    const heights = () =>
      page.locator("#chat-content [data-item-key]").evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().height)));
    const centred = await heights();
    await page.addStyleTag({ content: ".conversation-column { max-width: none !important; }" });
    await expect.poll(async () => (await page.locator("#chat-content").boundingBox())!.width, { message: "premise: the column is panel-wide again" }).toBeGreaterThan(1300);
    expect(await heights(), "row heights, centred and full-width").toEqual(centred);
  });

  test("Latest sits at the column's corner", async ({ page }) => {
    await openRoomWithComposer(page);
    await readerScrollsWithoutGesture(page, 0);
    const latest = page.getByTestId("scroll-to-bottom");
    await expect(latest).toBeVisible();
    // Its frame is centred in the panel, the history in the scroller's client
    // area: they differ by half a classic scrollbar, if any.
    const g = await page.evaluate(() => {
      const sc = document.getElementById("chat-scroll-container")!;
      const column = document.getElementById("chat-content")!.getBoundingClientRect();
      const b = document.querySelector('[data-testid="scroll-to-bottom"]')!.getBoundingClientRect();
      return { columnRight: column.right, buttonRight: b.right, halfScrollbar: (sc.offsetWidth - sc.clientWidth) / 2 };
    });
    expect(Math.abs(g.columnRight + g.halfScrollbar - g.buttonRight - 16), "16px in from the column's right edge").toBeLessThanOrEqual(1);
    // It still takes the click (its frame takes none).
    await latest.click();
    await expect(latest).toBeHidden();
  });
});

// Below a row's width nothing changes: at 1024 (both side panels, at their
// tablet widths) the panel is narrower than a row needs, and the column is
// exactly the panel, as before the centring. At 1280 the panel is within a
// few tens of px of a row's width (8px in Chromium, more where an engine's
// `ch` is narrower), so the column is the row's width with small equal gaps.
test.describe("narrower windows", () => {
  test("at 1024px the column is the whole panel, as before", async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await openRoomWithComposer(page);
    const m = await columnMetrics(page.getByTestId("message-row").last());
    expect(m.scrollerInner, "premise: the panel is narrower than a row needs").toBeLessThan(m.rowNeeds);
    expect(m.columnWidth).toBeCloseTo(m.scrollerInner, 0);
    expect(m.columnLeft).toBeCloseTo(0, 0);
  });

  test("at 1280px the column is at most a row wide, centred", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await openRoomWithComposer(page);
    const m = await columnMetrics(page.getByTestId("message-row").last());
    expect(m.columnWidth).toBeCloseTo(Math.min(m.scrollerInner, m.rowNeeds), 0);
    expect(Math.abs(m.columnLeft - m.columnRight)).toBeLessThanOrEqual(1);
  });
});

test.describe("Message headers", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the owner and deputy carry tags, and a similar name is labelled", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    const history = page.getByTestId("conversation-history");

    const ownerTag = history.getByTestId("message-author-owner-tag").first();
    await expect(ownerTag).toBeVisible({ timeout: 15_000 });
    await expect(ownerTag).toHaveText(/owner/i);
    await expect(ownerTag).toHaveAttribute("title", "Room Owner");
    await expect(ownerTag).toHaveAttribute("aria-label", "Room Owner");
    // Only on the owner's own header.
    const ownerNames = await history.getByTestId("message-author-owner-tag").evaluateAll((tags) =>
      tags.map((t) => t.parentElement!.querySelector('[data-testid="message-author-name"]')!.textContent),
    );
    // The owner is the member the panel marks with the crown.
    const owner = (await page.getByTestId("member-list").locator('[data-testid^="member-item-"] button')
      .filter({ has: page.getByTestId("member-list-owner") }).getByTestId("member-name").textContent())!.trim();
    expect(ownerNames.length).toBeGreaterThan(0);
    expect(ownerNames.every((n) => n!.trim() === owner), `${ownerNames.join(" | ")} vs ${owner}`).toBe(true);

    const deputy = history.getByTestId("message-author-deputy-badge").first();
    await expect(deputy).toHaveText(/deputy/i);
    await expect(deputy.locator("svg")).toHaveCount(1);

    // The ⚠ keeps its glyph, with a "Similar name" label right after it.
    const warning = history.getByTestId("message-author-impersonation-warning").first();
    await expect(warning).toHaveText("⚠");
    expect(await warning.evaluate((el) => el.nextElementSibling?.textContent)).toBe("Similar name");
    // In the warning colour, not the body text's.
    const [warn, text] = await Promise.all([
      warning.evaluate((el) => getComputedStyle(el.nextElementSibling!).color),
      resolveColor(page, "var(--color-text)"),
    ]);
    expect(await resolveColor(page, warn)).not.toEqual(text);
  });

  test("your own name is the link colour; others' names underline on hover instead", async ({ page }) => {
    test.skip(await coarsePointer(page), "no hover on a touch pointer");
    const { received, own } = await openRoomWithPairs(page);
    const [link, text] = [await resolveColor(page, "var(--color-link)"), await resolveColor(page, "var(--color-text)")];
    const ownName = own.getByTestId("message-author-name");
    const name = received.getByTestId("message-author-name");
    const style = (l: Locator) =>
      l.evaluate((el) => ({ color: getComputedStyle(el).color, line: getComputedStyle(el).textDecorationLine }));

    expect(await resolveColor(page, (await style(ownName)).color)).toEqual(link);
    expect((await style(name)).line).toBe("none");
    await name.hover();
    const hovered = await style(name);
    expect(hovered.line).toBe("underline");
    expect(await resolveColor(page, hovered.color), "others' names keep the text colour").toEqual(text);
  });
});

test.describe("Avatars", () => {
  test("neutral until the room's salt is known, then coloured by it", async ({ page }) => {
    const { received, own } = await openRoomWithPairs(page);
    const avatar = received.getByTestId("message-author-avatar");
    const disc = () => avatar.evaluate((el) => getComputedStyle(el).backgroundColor);
    const isGrey = async () => {
      const [r, g, b] = await resolveColor(page, await disc());
      return Math.max(r, g, b) - Math.min(r, g, b) <= 2;
    };

    await expect(avatar).toHaveText("TS");
    await callRiverTest(page, "setAvatarSalt", null);
    await expect.poll(isGrey, { message: "no salt: a neutral disc" }).toBe(true);
    await expect(avatar).not.toHaveAttribute("style", /--avatar-hue/);
    // And it stays neutral: nothing restarts a read that gave up here.
    await page.waitForTimeout(500);
    expect(await isGrey(), "still neutral").toBe(true);

    // Exact hues for the fixed-key sender (Test Sender Insert) under two
    // salts, pinned natively by `the_fixed_test_senders_hues_are_pinned`:
    // the same member, two viewers, two different colours.
    const hueOf = () => avatar.evaluate((el) => (el as HTMLElement).style.getPropertyValue("--avatar-hue").trim());
    await callRiverTest(page, "setAvatarSalt", 1);
    await expect.poll(hueOf).toBe("108");
    await callRiverTest(page, "setAvatarSalt", 2);
    await expect.poll(hueOf).toBe("288");

    await callRiverTest(page, "setAvatarSalt", 1);
    await expect.poll(isGrey, { message: "a salt colours the disc" }).toBe(false);
    const first = await disc();
    // Every author on screen is coloured, and the set changes with the salt.
    // (The exact difference is pinned above for one fixed key; the fixture's
    // other authors have random keys, so only the set is compared here.)
    const hues = () =>
      page.getByTestId("message-author-avatar").evaluateAll((els) => els.map((e) => (e as HTMLElement).style.getPropertyValue("--avatar-hue")));
    const underOne = await hues();
    await callRiverTest(page, "setAvatarSalt", 2);
    await expect.poll(hues).not.toEqual(underOne);
    expect(underOne.every((h) => h !== ""), "every listed author is coloured").toBe(true);
    expect(first).not.toBe("");

    // A read seed colours the disc before its salt is stored: render
    // computes the salt from the seed (`salt_for_render`). No salt is stored
    // (cleared first, and the hook stores none), so the colour can only come
    // from that fallback. (Not compared with a hue: the example room's key
    // is random per load, so the seed's hue is too.)
    await callRiverTest(page, "setAvatarSalt", null);
    await expect.poll(isGrey, { message: "cleared: neutral" }).toBe(true);
    await callRiverTest(page, "setSeedReadUnstored", 3);
    await expect(avatar).toHaveAttribute("style", /--avatar-hue/);
    await expect.poll(isGrey, { message: "a read seed colours the disc" }).toBe(false);

    // Your own avatar carries the accent ring.
    const ring = await own.getByTestId("message-author-avatar").evaluate((el) => getComputedStyle(el).outlineStyle);
    expect(ring).toBe("solid");
    expect(await avatar.evaluate((el) => getComputedStyle(el).outlineStyle)).toBe("none");
  });
});

// React has one way in per pointer: the hover bar's React with a mouse, the
// kebab menu's React on touch. A faint "+" after a message's reaction chips
// was a second way; Ian found two controls for one action untidy
// (2026-10-25), so it was removed. Its element stays only as the inert
// anchor the picker opens from, so it must never show or take a tap.
test.describe("Reaction row", () => {
  test("a message with reactions shows no add-reaction control after its chips", async ({ page }) => {
    const { own } = await openRoomWithPairs(page);
    const row = own.getByTestId("message-row").last();
    await reactTo(page, row);
    if (!(await coarsePointer(page))) await row.hover({ position: { x: 4, y: 4 } });

    const anchor = row.getByTestId("reaction-picker-anchor");
    await expect(anchor).toBeAttached();
    await expect(anchor).toBeHidden();
    const m = await anchor.evaluate((el) => {
      const r = el.getBoundingClientRect();
      return { w: r.width, h: r.height, events: getComputedStyle(el.parentElement!).pointerEvents, text: el.textContent };
    });
    expect(m).toEqual({ w: 0, h: 0, events: "none", text: "" });
    // The reaction line holds the chips and nothing else a reader can press.
    const reactionRow = row.getByTestId("message-reaction-row");
    await expect(reactionRow.getByTestId("reaction-chip")).toHaveCount(1);
    await expect(reactionRow.locator('button:not([data-testid="reaction-chip"]):visible')).toHaveCount(0);

    // The picker opens below or above the chips, never over them.
    await openInlinePicker(page, row);
    const picker = page.getByTestId("emoji-picker");
    await expect(picker).toBeVisible();
    const p = (await picker.boundingBox())!;
    const c = (await reactionRow.getByTestId("reaction-chip").boundingBox())!;
    const clear = p.y >= c.y + c.height - 0.5 || p.y + p.height <= c.y + 0.5;
    expect(clear, `picker ${p.y}..${p.y + p.height} vs chips ${c.y}..${c.y + c.height}`).toBe(true);
  });
});

// Chips that wrap onto several lines: the anchor shares only the last one,
// and a picker opening upward must clear the lines above it too.
test.describe("Reaction row that wraps, at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("the picker covers no chip on any line", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "appendMessage", "wrap target");
    const row = page.getByTestId("message-row").filter({ hasText: "wrap target" });
    await expect(row).toBeVisible();
    await callRiverTest(page, "addReactionsToNewest", ["👍", "❤️", "😂", "🔥", "🎉", "👀", "😮", "😢", "🙏", "💯"]);
    const chips = row.getByTestId("reaction-chip");
    await expect(chips).toHaveCount(10);
    await row.evaluate((el) => el.scrollIntoView({ block: "end" }));
    const lines = new Set((await chips.evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().top)))));
    expect(lines.size, "premise: the chips wrap").toBeGreaterThan(1);

    await openInlinePicker(page, row);
    const picker = page.getByTestId("emoji-picker");
    await expect(picker).toBeVisible();
    const p = (await picker.boundingBox())!;
    const overlaps = await chips.evaluateAll(
      (els, p) =>
        els.filter((e) => {
          const r = e.getBoundingClientRect();
          return r.left < p.x + p.width && r.right > p.x && r.top < p.y + p.height - 0.5 && r.bottom > p.y + 0.5;
        }).length,
      p,
    );
    expect(overlaps, "chips under the picker").toBe(0);
    // Premise: it opened upward, past the first line, so the rise mattered.
    expect(p.y + p.height, "the picker sits above the first chip line").toBeLessThanOrEqual(Math.min(...lines) + 0.5);
    expectEveryEmojiTappable(await measurePicker(page));
  });
});

test.describe("Join notices and date separators", () => {
  test("joins fold into one quiet line in the gutter; separators are not items", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "appendJoinEvents", 2);
    const notice = page.getByTestId("join-notice").last();
    await expect(notice).toContainText(/Test Sender [AB] and Test Sender [AB] joined · \d{1,2}:\d{2}/);
    await expect(notice).toHaveAttribute("data-item-key", /.+/);
    await expect(notice).toHaveAttribute("data-anchor-key", /.+/);
    const m = await notice.evaluate((el) => ({
      size: getComputedStyle(el).fontSize,
      iconInGutter: !!el.querySelector(".msg-gutter svg"),
      names: Array.from(el.querySelectorAll(".font-bold")).map((b) => b.textContent),
    }));
    expect(m.size).toBe("13px");
    expect(m.iconInGutter).toBe(true);
    expect(m.names).toHaveLength(2);

    const separator = page.getByTestId("date-separator").first();
    await expect(separator).toBeAttached();
    expect(await separator.getAttribute("data-item-key"), "a separator is not a history item").toBeNull();
    // A hairline each side of the label, not a pill.
    const rule = await separator.evaluate((el) => ({
      before: getComputedStyle(el, "::before").height,
      bg: getComputedStyle(el).backgroundColor,
    }));
    expect(rule.before).toBe("1px");
    expect(rule.bg).toBe("rgba(0, 0, 0, 0)");
  });

  // Ivvor: "8 people joined" should say who, to tell new members from
  // established ones. Hover shows the names; a tap or click lists them.
  test("a count of joins names who on hover and when opened", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "appendJoinEvents", 3);
    const notice = page.getByTestId("join-notice").last();
    const toggle = notice.getByTestId("join-notice-toggle");
    const list = notice.getByTestId("join-notice-names");
    const who = /^Test Sender [AB], Test Sender [AB], Test Sender [AB]$/;
    await expect(toggle).toContainText(/^3 people joined · \d{1,2}:\d{2}/);
    await expect(toggle).toHaveAttribute("title", who);
    await expect(list).toBeHidden();

    await toggle.click();
    await expect(list).toBeVisible();
    await expect(list).toHaveText(who);
    // One element per name, so a nickname holding ", " reads as one person.
    await expect(list.locator(".join-name")).toHaveCount(3);

    // A re-render (another join folding in) keeps it open and adds the name.
    await callRiverTest(page, "appendJoinEvents", 1);
    await expect(toggle).toContainText(/^4 people joined/);
    await expect(list).toBeVisible();
    await expect(list).toHaveText(/^(Test Sender [AB], ){3}Test Sender [AB]$/);

    await toggle.click();
    await expect(list).toBeHidden();
  });
});

// The narrowest supported width: a long message, a long link, a reply and a
// reaction row must wrap inside the content column, and the controls must stay
// on screen. Regression (#212): a long unbreakable URL must wrap rather than
// widen the row or the page. The example data carries that URL and a reply
// quoting it, whose strip preview must stay in the column too.
test.describe("Message rows at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("nothing overflows the column and the controls stay reachable", async ({ page }) => {
    const { own } = await openRoomWithPairs(page);
    // The example room's fixtures: a long unbreakable link and a reply quoting it.
    await expect(page.getByTestId("message-body").filter({ hasText: "longlongurlpath" }).first()).toBeAttached();
    await expect(page.locator(".reply-strip").filter({ hasText: "longlongurl" }).first()).toBeAttached();
    // A send drops the fixture reactions, so react to the newest own message.
    await reactTo(page, own.getByTestId("message-row").last());

    const report = await page.evaluate(() => {
      const vw = document.documentElement.clientWidth;
      const scroller = document.getElementById("chat-scroll-container")!;
      const sr = scroller.getBoundingClientRect();
      const bad: string[] = [];
      const label = (e: Element) => (e.textContent || "").trim().replace(/\s+/g, " ").slice(0, 30);
      for (const row of Array.from(document.querySelectorAll('[data-testid="message-row"]'))) {
        const r = row.getBoundingClientRect();
        if (r.left < sr.left - 0.5 || r.right > sr.right + 0.5) bad.push(`row ${label(row)}: ${r.left}..${r.right}`);
        const column = row.querySelector('[data-testid="message-content"]')!.getBoundingClientRect().right;
        for (const sel of ['[data-testid="message-body"]', ".reply-strip", ".reply-strip-unavailable", '[data-testid="message-reaction-row"]', '[data-testid="message-group-header"]']) {
          for (const part of Array.from(row.querySelectorAll(sel))) {
            const p = part.getBoundingClientRect();
            if (p.right > column + 0.5) bad.push(`${sel} ${label(part)} ends at ${p.right}, column at ${column}`);
            if ((part as HTMLElement).scrollWidth > (part as HTMLElement).clientWidth + 1 && !part.classList.contains("reply-strip")) {
              bad.push(`${sel} ${label(part)} overflows its own box`);
            }
          }
        }
      }
      return {
        bad,
        docOverflow: document.documentElement.scrollWidth - vw,
        scrollerOverflow: scroller.scrollWidth - scroller.clientWidth,
      };
    });
    expect(report.bad).toEqual([]);
    expect(report.docOverflow).toBeLessThanOrEqual(0);
    expect(report.scrollerOverflow).toBeLessThanOrEqual(0);

    // The newest own message's controls stay inside the scroller: the hover
    // bar over the row, the touch kebab in the gutter past the content.
    const row = own.getByTestId("message-row").last();
    const content = (await row.getByTestId("message-content").boundingBox())!;
    const scroller = (await page.locator("#chat-scroll-container").boundingBox())!;
    const kebab = row.getByTestId("message-kebab");
    if (await touchOnly(page)) {
      const b = (await kebab.boundingBox())!;
      expect(b.x).toBeGreaterThanOrEqual(content.x + content.width - 0.5);
      expect(b.x + b.width).toBeLessThanOrEqual(scroller.x + scroller.width + 0.5);
      await kebab.click({ trial: true });
    } else {
      await row.hover();
      const b = (await row.getByTestId("message-hover-actions").boundingBox())!;
      expect(b.x).toBeGreaterThanOrEqual(scroller.x - 0.5);
      expect(b.x + b.width).toBeLessThanOrEqual(scroller.x + scroller.width + 0.5);
    }
  });
});

// The inline picker hangs off the anchor after a row's chips, and the scroller
// clips its sides, so at 320px it opens toward the roomier side. Each case
// checks every emoji is inside the scroller and takes a tap at its centre.
type PickerReport = Awaited<ReturnType<typeof measurePicker>>;

/// The open picker, its anchor's wrapper (the picker's containing block) and the
/// scroller, with no click in between: a click elsewhere would close it.
async function measurePicker(page: Page) {
  await expect(page.getByTestId("emoji-picker")).toBeVisible();
  return page.evaluate(() => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    const scroller = box(document.getElementById("chat-scroll-container")!);
    const picker = document.querySelector('[data-testid="emoji-picker"]')!;
    const anchorEl = picker.parentElement!.querySelector('[data-testid="reaction-picker-anchor"]')!;
    const buttons = Array.from(picker.querySelectorAll("button")).map((b) => {
      const r = b.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {
        emoji: (b.textContent || "").trim(),
        inside: r.left >= scroller.left - 0.5 && r.right <= scroller.right + 0.5 &&
          r.top >= scroller.top - 0.5 && r.bottom <= scroller.bottom + 0.5,
        hit: at !== null && (at === b || b.contains(at)),
      };
    });
    return { scroller, anchor: box(anchorEl.parentElement!), picker: box(picker), buttons };
  });
}

function expectEveryEmojiTappable(report: PickerReport) {
  expect(report.buttons.length, "the picker has emoji").toBeGreaterThan(0);
  const outside = report.buttons.find((b) => !b.inside);
  expect(outside, `${outside?.emoji} is clipped by the scroller (picker ${report.picker.left}..${report.picker.right} x ${report.picker.top}..${report.picker.bottom}, scroller ${report.scroller.left}..${report.scroller.right} x ${report.scroller.top}..${report.scroller.bottom})`).toBeUndefined();
  const covered = report.buttons.find((b) => !b.hit);
  expect(covered, `${covered?.emoji} is not the element a tap at its centre lands on`).toBeUndefined();
}

/// The newest message, "picker edge", with as many reaction chips as leave its
/// the anchor on their line, so the anchor sits near the column's right edge. A fixed count
/// does not do it: four chips left the anchor at 160px in Chromium, where a 140px
/// picker still fit, and six wrapped the anchor to the line's start in WebKit, whose
/// emoji chips are 3px wider.
async function nearEdgeRow(page: Page): Promise<Locator> {
  const emojis = ["👍", "❤️", "😂", "🔥", "🎉", "👀", "😮", "😢", "🙏", "💯"];
  await openRoomWithComposer(page);
  await deliverOffscreen(page, "picker edge");
  const row = page.getByTestId("message-row").filter({ hasText: "picker edge" });
  const chips = row.getByTestId("reaction-chip");
  await callRiverTest(page, "addReactionsToNewest", emojis.slice(0, 1));
  await expect(chips).toHaveCount(1);
  const fit = await row.evaluate((el) => {
    const chip = el.querySelector('[data-testid="reaction-chip"]')!.getBoundingClientRect();
    const anchorEl = el.querySelector('[data-testid="reaction-picker-anchor"]')!.getBoundingClientRect();
    const line = el.querySelector('[data-testid="message-reaction-row"]')!.getBoundingClientRect();
    // Each chip moves the anchor one chip and its gap further right.
    return Math.floor((line.right - anchorEl.right) / (anchorEl.left - chip.left)) + 1;
  });
  expect(fit, "chips that fit one line").toBeLessThanOrEqual(emojis.length);
  await callRiverTest(page, "addReactionsToNewest", emojis.slice(1, fit));
  await expect(chips).toHaveCount(fit);
  await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
  return row;
}

/// What `left-0` would do: a rightward picker from this anchor ends past the scroller.
function expectRightwardWouldClip(report: PickerReport) {
  expect(report.anchor.left + width(report.picker), "premise: a rightward picker would clip").toBeGreaterThan(report.scroller.right);
}

/// The scroll offsets a reveal must leave alone: the history's horizontal
/// one, the app root's and the page's (all `overflow: hidden`).
function strayScroll(page: Page) {
  return page.evaluate(() => ({
    historyLeft: document.getElementById("chat-scroll-container")!.scrollLeft,
    root: [document.querySelector(".app-root")!.scrollTop, document.querySelector(".app-root")!.scrollLeft],
    page: [document.scrollingElement!.scrollTop, document.scrollingElement!.scrollLeft],
  }));
}
const NO_STRAY_SCROLL = { historyLeft: 0, root: [0, 0], page: [0, 0] };

// A popover that overflows sideways, on a short phone view, is revealed by
// the history's scrollTop alone: `scrollIntoView` also scrolled the
// history's scrollLeft and overflow-hidden ancestors, which nothing scrolls
// back. The overflow is forced (the picker and the form fit at every real
// width), so the old behaviour would show.
test.describe("Revealing a popover on a short phone view", () => {
  test.use({ viewport: { width: 320, height: 480 } });

  test("the picker and the edit form scroll nothing sideways or outside the history", async ({ page }) => {
    await openRoomWithComposer(page);
    await page.addStyleTag({
      content: '[data-testid="emoji-picker"] { min-width: 520px; min-height: 700px; } [id^="edit-form-"] { min-width: 600px; }',
    });
    const row = page.getByTestId("message-row").last();
    await openInlinePicker(page, row);
    await expect(page.getByTestId("emoji-picker")).toBeVisible();
    await nextFrames(page);
    expect(await strayScroll(page), "picker").toEqual(NO_STRAY_SCROLL);
    await page.keyboard.press("Escape");
    await page.mouse.click(2, 2);
    await expect(page.getByTestId("emoji-picker")).toHaveCount(0);

    await openOwnMessageEdit(page);
    await nextFrames(page);
    expect(await strayScroll(page), "edit form").toEqual(NO_STRAY_SCROLL);
  });
});

test.describe("Reaction picker at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("near the right edge, the picker opens leftward and every emoji is tappable", async ({ page }) => {
    const row = await nearEdgeRow(page);
    await openInlinePicker(page, row);
    const report = await measurePicker(page);
    expectRightwardWouldClip(report);
    expectEveryEmojiTappable(report);
  });

  test("at the start of the line, the picker opens rightward", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliverOffscreen(page, "picker start");
    const row = page.getByTestId("message-row").filter({ hasText: "picker start" });
    await expect(row.getByTestId("reaction-chip")).toHaveCount(0);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await openInlinePicker(page, row);
    const report = await measurePicker(page);
    // What an unconditional `right-0` would do: a leftward picker from this anchor starts before the scroller.
    expect(report.anchor.right - width(report.picker), "premise: + starts the line").toBeLessThan(report.scroller.left);
    expectEveryEmojiTappable(report);
  });

  // #791 review: the kebab's React lost its test when the gutter moved.
  test("the kebab's React opens the picker on the side its \"+\" would", async ({ page }) => {
    test.skip(!(await touchOnly(page)), "the kebab is the touch path");
    const row = await nearEdgeRow(page);
    await row.getByTestId("message-kebab").click();
    await page.getByTestId("message-action-menu").getByRole("button", { name: "React" }).click();
    const report = await measurePicker(page);
    expectRightwardWouldClip(report);
    expectEveryEmojiTappable(report);
  });
});

// #791 review: the picker opened from the keyboard reports no click
// position, so its side must come from the anchor it hangs off. The newest
// message, at the bottom of the view, has no room for the picker below it.
test("the picker opened from the keyboard at the bottom of the view opens upward", async ({ page }) => {
  test.skip(await touchOnly(page), "no hover bar, so no keyboard path, on touch");
  await openRoomWithComposer(page);
  await deliver(page, "keyboard picker");
  const row = page.getByTestId("message-row").filter({ hasText: "keyboard picker" });
  await expect(row).toBeInViewport();
  const room = await row.evaluate((el) => {
    const r = el.querySelector('[data-testid="reaction-picker-anchor"]')!.parentElement!.getBoundingClientRect();
    const s = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return { below: s.bottom - r.bottom, above: r.top - s.top };
  });
  expect(room.below, "premise: no room below for the picker").toBeLessThan(140);
  expect(room.above, "premise: room above").toBeGreaterThan(140);
  await page.mouse.move(0, 0);
  await row.getByTestId("message-hover-actions").getByRole("button", { name: "React" }).focus();
  await page.keyboard.press("Enter");
  const report = await measurePicker(page);
  expect(report.picker.bottom, "the picker opens above its anchor").toBeLessThanOrEqual(report.anchor.top + 0.5);
  expectEveryEmojiTappable(report);
});

// The hover band (fine pointers only): hovering anywhere in a message row
// fills that row and only that row; your own rows keep a faint tint at rest
// and take a stronger one under the pointer; the band stays while a control
// the row opened is expanded.
function background(row: Locator) {
  return row.evaluate(async (el) => {
    // Let the band's fade settle: a poll would pass on a sample mid-fade.
    await Promise.all(el.getAnimations().map((a) => a.finished));
    return getComputedStyle(el).backgroundColor;
  });
}

/// The ids of rows whose background is not their resting one: none for
/// others' rows, the own-row tint for yours.
async function bandedRows(page: Page): Promise<string[]> {
  return page.getByTestId("message-row").evaluateAll((rows) => {
    const probe = document.createElement("div");
    probe.style.backgroundColor = "var(--color-row-self)";
    document.body.appendChild(probe);
    const tint = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return rows
      .filter((r) => {
        const rest = r.getAttribute("data-message-self") === "true" ? tint : "rgba(0, 0, 0, 0)";
        return getComputedStyle(r).backgroundColor !== rest;
      })
      .map((r) => r.id);
  });
}

test.describe("Message row hover band", () => {
  test.beforeEach(async ({ page }) => test.skip(await coarsePointer(page), "no hover band on a touch pointer"));

  test("only the hovered row lights, from its header or a continuation row, own rows included", async ({ page }) => {
    const { received, own } = await openRoomWithPairs(page);
    const band = await resolveColor(page, "var(--color-row-hover)");
    const ownBand = await resolveColor(page, "var(--color-row-self-hover)");
    expect(band[3], "premise: the band token resolves to a visible colour").toBeGreaterThan(0);
    expect(ownBand, "premise: the own band differs from the others'").not.toEqual(band);
    const [first, second] = [received.getByTestId("message-row").first(), received.getByTestId("message-row").nth(1)];
    const ownSecond = own.getByTestId("message-row").nth(1);

    for (const [what, target, row, colour] of [
      ["the header", received.getByTestId("message-group-header"), first, band],
      ["a continuation row", second.getByTestId("message-body"), second, band],
      ["an own row", ownSecond.getByTestId("message-body"), ownSecond, ownBand],
    ] as const) {
      await page.mouse.move(0, 0);
      await expect.poll(() => bandedRows(page), { message: "idle: every row at rest" }).toEqual([]);
      await target.hover();
      await expect
        .poll(async () => resolveColor(page, await background(row)), { message: `${what}: band` })
        .toEqual(colour);
      expect(await bandedRows(page), `${what}: only its row`).toEqual([await row.getAttribute("id")]);
    }
  });

  // #791 review: the plain hover band shared one selector list with `:has()`,
  // so a browser without `:has()` dropped both. The hover rules stand alone.
  test("the hover band does not depend on :has()", async ({ page }) => {
    await openRoomWithComposer(page);
    const selectors = await page.evaluate(() => {
      const out: string[] = [];
      const walk = (rules: CSSRuleList) => {
        for (const rule of Array.from(rules)) {
          if (rule instanceof CSSStyleRule) out.push(rule.selectorText);
          if ("cssRules" in rule) walk((rule as CSSGroupingRule).cssRules);
        }
      };
      for (const sheet of Array.from(document.styleSheets)) {
        try {
          walk(sheet.cssRules);
        } catch {
          // A cross-origin sheet; the app ships none.
        }
      }
      return out.filter((s) => /\.msg-row(\[[^\]]*\])?:hover(?![\w-])/.test(s) && !s.includes(".hover-actions") && !s.includes(".msg-gutter-time"));
    });
    expect(selectors.length, "premise: the hover band rules were found").toBeGreaterThanOrEqual(2);
    for (const s of selectors) expect(s, "a hover band rule names :has()").not.toContain(":has(");
  });

  test("a continuation row shows its time in the gutter under the pointer", async ({ page }) => {
    const { received } = await openRoomWithPairs(page);
    const second = received.getByTestId("message-row").nth(1);
    const time = second.getByTestId("message-gutter-time");
    await expect(time).toHaveText(/^\d{1,2}:\d{2}/);
    const opacity = () => time.evaluate(async (el) => {
      await Promise.all(el.getAnimations().map((a) => a.finished));
      return getComputedStyle(el).opacity;
    });
    await page.mouse.move(0, 0);
    expect(await opacity()).toBe("0");
    await second.getByTestId("message-body").hover();
    await expect.poll(opacity).toBe("1");
  });

  test.describe("at a narrow width", () => {
    test.use({ viewport: { width: 480, height: 800 } });

    test("the band stays while the row's reactor list is open, and removing the reaction does not scroll", async ({ page }) => {
      const { received } = await openRoomWithPairs(page);
      const row = received.getByTestId("message-row").nth(1);
      await row.scrollIntoViewIfNeeded();
      const chip = await reactTo(page, row);

      const band = await resolveColor(page, "var(--color-row-hover)");
      // Park the reader short of the end, where nothing but a scroll moves
      // scrollTop: at the end, the reaction line collapsing would clamp it.
      // Short by more than that line, and not so far that the chip leaves the
      // view (focusing it would scroll it back).
      const slack = await chip.evaluate((el) => {
        const view = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
        return view.bottom - el.getBoundingClientRect().bottom - 8;
      });
      const short = Math.min(150, slack);
      expect(short, "premise: room to park short of the end").toBeGreaterThan(40);
      await readerScrollsWithoutGesture(page, (await scrollMetrics(page)).max - short);
      await expect(chip).toBeInViewport({ ratio: 1 });
      await chip.focus();
      await page.keyboard.press("Enter");
      const owners = page.getByTestId("reaction-owners");
      await expect(owners).toBeVisible();
      await expect(chip).toHaveAttribute("aria-expanded", "true");
      // The list's dismiss backdrop sits inside the row, so with the pointer
      // anywhere over it the row is `:hover` and the band would show without
      // the `:has()` rule. Take the backdrop out of hit-testing, so only that
      // rule can paint the band.
      await row.evaluate((el) => {
        for (const b of Array.from(el.querySelectorAll<HTMLElement>(".fixed.inset-0"))) b.style.pointerEvents = "none";
      });
      await page.mouse.move(0, 0);
      expect(await row.evaluate((el) => el.matches(":hover")), "premise: the row is not hovered").toBe(false);
      expect(await resolveColor(page, await background(row)), "the band stays while the list is open").toEqual(band);
      const box = (await owners.boundingBox())!;
      expect(box.x).toBeGreaterThanOrEqual(-0.5);
      expect(box.x + box.width).toBeLessThanOrEqual(480.5);

      // Remove the reaction from the list: the chip and the band go with it,
      // and nothing scrolls.
      const before = (await scrollMetrics(page)).top;
      await page.getByTestId("reaction-remove").click();
      await expect(chip).toHaveCount(0);
      await expect(owners).toHaveCount(0);
      await page.mouse.move(0, 0);
      await expect.poll(() => bandedRows(page), { message: "the band goes with the list" }).toEqual([]);
      expect((await scrollMetrics(page)).top, "removing the reaction does not scroll").toBe(before);
    });
  });
});

// The hover bar is invisible at rest AND takes no hits, or it would take
// clicks meant for whatever is under it (Latest, the row above). Hovering its
// row, or keyboard focus inside the row, shows it and makes it clickable.
test.describe("Hover bar", () => {
  test("idle it takes no hits; hover or keyboard focus reveals it", async ({ page }) => {
    const touch = await touchOnly(page);
    const { received } = await openRoomWithPairs(page);
    const row = received.getByTestId("message-row").nth(1);
    const bar = row.getByTestId("message-hover-actions");
    if (touch) {
      // No hover pointer: the bar is not displayed at all; the kebab replaces it.
      await expect(bar).toBeHidden();
      await expect(row.getByTestId("message-kebab")).toBeVisible();
      return;
    }
    const state = () =>
      bar.evaluate(async (el) => {
        await Promise.all(el.getAnimations().map((a) => a.finished));
        const r = el.getBoundingClientRect();
        const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return { opacity: getComputedStyle(el).opacity, events: getComputedStyle(el).pointerEvents, hit: !!at && el.contains(at) };
      });

    await row.scrollIntoViewIfNeeded();
    await page.mouse.move(0, 0);
    expect(await state(), "idle").toEqual({ opacity: "0", events: "none", hit: false });

    await row.getByTestId("message-body").hover();
    await expect.poll(state, { message: "hovered" }).toEqual({ opacity: "1", events: "auto", hit: true });

    await page.mouse.move(0, 0);
    await expect.poll(state).toEqual({ opacity: "0", events: "none", hit: false });
    // Focus the bar's first button, where Tab lands from the row's content,
    // as a keyboard user does.
    await keyboardFocus(page, bar.getByRole("button", { name: "React" }));
    await expect.poll(state, { message: "focus within the row" }).toMatchObject({ opacity: "1", events: "auto" });
    expect(await bar.getByRole("button", { name: "React" }).evaluate((el) => el === document.activeElement)).toBe(true);
  });
  test("a mouse click inside the row does not keep the bar after the pointer leaves", async ({ page }) => {
    test.skip(await touchOnly(page), "no hover bar on touch");
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    const row = page.getByTestId("message-row").filter({ has: page.getByTestId("reply-strip") }).first();
    await row.scrollIntoViewIfNeeded();
    const bar = row.getByTestId("message-hover-actions");
    const strip = row.getByTestId("reply-strip");
    const state = () =>
      bar.evaluate(async (el) => {
        await Promise.all(el.getAnimations().map((a) => a.finished));
        return { opacity: getComputedStyle(el).opacity, events: getComputedStyle(el).pointerEvents };
      });
    // A mouse click focuses the strip (it is a tabindex="0" button).
    await strip.click();
    await expect.poll(() => strip.evaluate((el) => el === document.activeElement), { message: "premise: the click focused the strip" }).toBe(true);
    await page.mouse.move(0, 0);
    await expect.poll(state, { message: "pointer gone, focus from a mouse click" }).toEqual({ opacity: "0", events: "none" });
    // The same focus from the keyboard does reveal it.
    await strip.evaluate((el) => (el as HTMLElement).blur());
    await keyboardFocus(page, strip);
    await expect.poll(() => strip.evaluate((el) => el.matches(":focus-visible")), { message: "premise: keyboard focus" }).toBe(true);
    await expect.poll(state, { message: "keyboard focus in the row" }).toEqual({ opacity: "1", events: "auto" });
  });
});

// React on a message taller than the view: the picker hangs off the anchor at
// the message's foot, below the fold when React is taken from the top of the
// message. Opening brings it into view, and the next click is the reader's
// pick, not swallowed by the invisible backdrop.
test.describe("React on a tall message", () => {
  test("the picker opens in view and takes the next click", async ({ page }) => {
    await openRoomWithComposer(page);
    // Tall but well under the room's message-size limit (an over-size
    // message is dropped at the next send, the reaction's).
    const text = Array.from({ length: 40 }, (_, i) => `tall ${i}`).join("\n\n") + "\n\ntall paragraph 59";
    await callRiverTest(page, "appendMessage", text);
    const row = page.getByTestId("message-row").filter({ hasText: "tall paragraph 59" });
    await expect(row).toHaveCount(1);
    // Read from the message's top: its foot is far below the view.
    await row.evaluate((el) => {
      const c = document.getElementById("chat-scroll-container")!;
      c.scrollTop += el.getBoundingClientRect().top - c.getBoundingClientRect().top - 40;
    });
    const geometry = () =>
      page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
        const p = document.querySelector('[data-testid="emoji-picker"]')?.getBoundingClientRect();
        const anchorEl = Array.from(document.querySelectorAll('[data-testid="message-row"]'))
          .find((r) => r.textContent!.includes("tall paragraph 59"))!
          .querySelector('[data-testid="reaction-picker-anchor"]')!
          .parentElement!.getBoundingClientRect();
        return { view: [c.top, c.bottom], picker: p ? [p.top, p.bottom] : null, anchorTop: anchorEl.top };
      });
    expect((await geometry()).anchorTop, "premise: the message's foot is below the view").toBeGreaterThan((await geometry()).view[1]);

    await openInlinePicker(page, row);
    const picker = page.getByTestId("emoji-picker");
    await expect(picker).toBeVisible();
    await expect
      .poll(async () => {
        const g = await geometry();
        return !!g.picker && g.picker[0] >= g.view[0] - 1 && g.picker[1] <= g.view[1] + 1;
      }, { message: "the picker is wholly inside the history's view" })
      .toBe(true);
    // Opened once, revealed once: with the picker open, the reader moves up,
    // and a re-render of that row (a reaction added) and an arrival leave
    // the view where the reader put it.
    const scroller = page.locator("#chat-scroll-container");
    await scroller.evaluate((c) => (c.scrollTop -= 160));
    await page.waitForTimeout(150);
    const parked = await scroller.evaluate((c) => c.scrollTop);
    await callRiverTest(page, "addReactionsToNewest", ["🎉"]);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);
    await page.waitForTimeout(300);
    expect(Math.abs((await scroller.evaluate((c) => c.scrollTop)) - parked), "no second reveal").toBeLessThanOrEqual(2);
    await page.mouse.move(0, 0);
    await page.keyboard.press("Escape");
  });

  test("the picker takes the next click after the reveal", async ({ page }) => {
    await openRoomWithComposer(page);
    const text = Array.from({ length: 40 }, (_, i) => `tall ${i}`).join("\n\n") + "\n\ntall paragraph 59";
    await callRiverTest(page, "appendMessage", text);
    const row = page.getByTestId("message-row").filter({ hasText: "tall paragraph 59" });
    await expect(row).toHaveCount(1);
    await row.evaluate((el) => {
      const c = document.getElementById("chat-scroll-container")!;
      c.scrollTop += el.getBoundingClientRect().top - c.getBoundingClientRect().top - 40;
    });
    await openInlinePicker(page, row);
    const picker = page.getByTestId("emoji-picker");
    await expect(picker).toBeVisible();
    await picker.getByRole("button").first().click();
    await expect(picker).toHaveCount(0);
    await expect(row.getByTestId("reaction-chip"), "the click reacted").toHaveCount(1);
  });
});

/// Focus `target` as a keyboard user would, so it is `:focus-visible`. A key
/// press then a scripted focus does it in Chromium and WebKit (which does not
/// Tab to every control); Firefox needs a real Tab, so step off and back.
async function keyboardFocus(page: Page, target: Locator) {
  await page.keyboard.press("Escape");
  await target.focus();
  if (!(await target.evaluate((el) => el.matches(":focus-visible")))) {
    await page.keyboard.press("Shift+Tab");
    await page.keyboard.press("Tab");
  }
  await expect.poll(() => target.evaluate((el) => el === document.activeElement && el.matches(":focus-visible")), {
    message: "premise: keyboard focus",
  }).toBe(true);
}

// A row's resting controls (the hover bar on a hover pointer, the kebab on
// touch) can share Latest's corner. They paint under Latest, so a row at the
// bottom of the view never takes Latest's click.
test.describe("Latest over row controls", () => {
  test("a row control aligned with Latest never takes its click", async ({ page }) => {
    const touch = await touchOnly(page);
    await page.setViewportSize(touch ? { width: 390, height: 800 } : { width: 1024, height: 800 });
    await openRoomWithComposer(page);
    const control = touch ? '[data-testid="message-kebab"]' : '[data-testid="message-hover-actions"]';
    await readerLeavesTheEnd(page);
    const latest = page.getByTestId("scroll-to-bottom");
    await expect(latest).toBeVisible();
    // Scroll the row nearest mid-view so its control's centre is on Latest's centre.
    const { id, target } = await page.evaluate((sel) => {
      const scroller = document.getElementById("chat-scroll-container")!;
      const l = document.querySelector('[data-testid="scroll-to-bottom"]')!.getBoundingClientRect();
      const mid = scroller.getBoundingClientRect().top + scroller.clientHeight / 2;
      const rows = [...document.querySelectorAll<HTMLElement>('[data-testid="message-row"]')];
      const dist = (r: HTMLElement) => Math.abs(r.getBoundingClientRect().top - mid);
      const row = rows.reduce((a, b) => (dist(b) < dist(a) ? b : a));
      const c = row.querySelector(sel)!.getBoundingClientRect();
      return { id: row.id, target: scroller.scrollTop + c.top + c.height / 2 - (l.top + l.height / 2) };
    }, control);
    await readerScrollsWithoutGesture(page, target);
    const row = page.locator(`[id="${id}"]`);
    if (!touch) await row.getByTestId("message-content").hover({ position: { x: 4, y: 4 } });
    // Premise: the control really overlaps Latest, or the test proves nothing.
    const [c, l] = [(await row.locator(control).boundingBox())!, (await latest.boundingBox())!];
    const [left, right] = [Math.max(c.x, l.x), Math.min(c.x + c.width, l.x + l.width)];
    const [top, bottom] = [Math.max(c.y, l.y), Math.min(c.y + c.height, l.y + l.height)];
    expect(right - left, "premise: the control overlaps Latest across").toBeGreaterThan(0);
    expect(bottom - top, "premise: the control overlaps Latest down").toBeGreaterThan(0);
    // Playwright refuses the click if the control would take it there.
    await latest.click({ position: { x: (left + right) / 2 - l.x, y: (top + bottom) / 2 - l.y } });
    await expect(latest).toBeHidden();
  });
});

// The hover bar ends at the text measure, not the row's far edge, so it
// stays next to the message it acts on: in the centred column (where the
// row's content is the measure) and wherever a row is wider than that (here
// forced, by lifting the column's cap).
for (const wide of [false, true]) {
  test(`the hover bar ends at the text column on a wide window${wide ? ", in a row wider than the measure" : ""}`, async ({ page }) => {
    test.skip(await touchOnly(page), "touch has no hover bar");
    await page.setViewportSize({ width: 1920, height: 900 });
    const { received } = await openRoomWithPairs(page);
    if (wide) await page.addStyleTag({ content: ".conversation-column { max-width: none !important; }" });
    const row = received.getByTestId("message-row").last();
    await row.hover();
    const bar = (await row.getByTestId("message-hover-actions").boundingBox())!;
    const m = await row.evaluate((el) => {
      const body = el.querySelector('[data-testid="message-body"]')!;
      const content = el.querySelector('[data-testid="message-content"]')!.getBoundingClientRect();
      return {
        measureRight: body.getBoundingClientRect().left + parseFloat(getComputedStyle(body).maxWidth),
        contentRight: content.right,
      };
    });
    if (wide) expect(m.contentRight - m.measureRight, "premise: the row is wider than the text measure").toBeGreaterThan(100);
    expect(Math.abs(bar.x + bar.width - m.measureRight), "bar's right edge at the measure").toBeLessThanOrEqual(1);
  });
}

// Opening a room or receiving a message re-renders the conversation, and a
// message group whose data did not change must not render again with it. The
// groups' event handlers used to be new closures on every render, which never
// compare equal, so every group re-rendered three or four times on opening a
// room: most of the delay on clicking a busy room.
test.describe("Message group renders", () => {
  async function groupCount(page: Page): Promise<number> {
    return page.getByTestId("conversation-history").locator(".msg-group").count();
  }

  test("opening a room renders each group once", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Public Discussion Room");
    await nextFrames(page);
    await callRiverTest(page, "takeMessageGroupRenders");
    await selectRoom(page, "Team Chat Room");
    await nextFrames(page);
    await page.waitForTimeout(300);
    // The wait above lets the opening settle, so no group mounts or unmounts
    // between these two reads.
    const renders = await callRiverTest(page, "takeMessageGroupRenders");
    const groups = await groupCount(page);
    expect(groups, "premise: the room has enough groups to tell one render from several").toBeGreaterThanOrEqual(5);
    expect(renders, `renders for ${groups} groups`).toBe(groups);
  });

  test("an arrival renders only the group it changes", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await nextFrames(page);
    await page.waitForTimeout(300);
    expect(await groupCount(page), "premise: groups an arrival leaves alone").toBeGreaterThanOrEqual(5);
    // The hook alternates between two senders and admits each on first use. A
    // new member changes the names every group is given, so both are admitted
    // before the measured arrival.
    for (const text of ["group render warm-up A", "group render warm-up B"]) {
      await callRiverTest(page, "appendMessage", text);
      await expect(page.getByTestId("conversation-history")).toContainText(text);
    }
    await nextFrames(page);
    await callRiverTest(page, "takeMessageGroupRenders");
    await callRiverTest(page, "appendMessage", "group render probe");
    await expect(page.getByTestId("conversation-history")).toContainText("group render probe");
    await nextFrames(page);
    await page.waitForTimeout(300);
    // The new message's group, plus the group above it when the arrival
    // changes how that one ends.
    const renders = await callRiverTest(page, "takeMessageGroupRenders");
    expect(renders, "the new message's group renders").toBeGreaterThanOrEqual(1);
    expect(renders).toBeLessThanOrEqual(2);
  });

  // Reacting still works on an older group after an arrival the groups above
  // it were skipped for. (A skipped child's handlers are re-pointed by Dioxus,
  // and `handle_toggle_reaction` reads the room when called, so this checks
  // the behaviour, not which mechanism keeps the handler current.)
  test("a group skipped by a render still reacts", async ({ page }) => {
    await openRoomWithComposer(page);
    for (const text of ["skip probe A", "skip probe B", "skip probe C"]) {
      await callRiverTest(page, "appendMessage", text);
      await expect(page.getByTestId("conversation-history")).toContainText(text);
    }
    await nextFrames(page);
    await callRiverTest(page, "takeMessageGroupRenders");
    await callRiverTest(page, "appendMessage", "skip probe D");
    await expect(page.getByTestId("conversation-history")).toContainText("skip probe D");
    await nextFrames(page);
    const row = page.getByTestId("message-row").filter({ hasText: "skip probe B" });
    expect(await callRiverTest(page, "takeMessageGroupRenders"), "premise: the arrival skipped the reacted group").toBeLessThanOrEqual(2);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(0);
    await reactTo(page, row);
  });
});

// Reply builds the quoted preview when it is clicked rather than on every
// render: the preview is the message cleaned of markdown, then truncated.
test("Reply quotes the message as plain text", async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "appendMessage", "**reply probe** with [a link](https://example.com/x)");
  const row = page.getByTestId("message-row").filter({ hasText: "reply probe" }).last();
  await expect(row).toBeVisible();
  if (await touchOnly(page)) {
    await row.getByTestId("message-kebab").click();
    await page.getByTestId("message-action-menu").getByRole("button", { name: "Reply" }).click();
  } else {
    await row.getByTestId("message-body").hover();
    await row.getByRole("button", { name: "Reply" }).click();
  }
  const composer = page.getByTestId("message-composer");
  await expect(composer).toContainText("Replying to");
  await expect(composer).toContainText("reply probe with a link");
  await expect(composer).not.toContainText("**");
  await expect(composer).not.toContainText("example.com");
});

// The edit form's @ list is built only while a message is being edited.
test("the edit form offers members to mention", async ({ page }) => {
  await openRoomWithComposer(page);
  const editArea = await openOwnMessageEdit(page);
  await editArea.press("End");
  await editArea.pressSequentially(" @");
  await expect(page.getByRole("listbox").getByRole("option").first()).toBeVisible({ timeout: 5_000 });
});
