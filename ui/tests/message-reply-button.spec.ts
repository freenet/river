import { test, expect, Page, Locator } from "@playwright/test";
import { selectListedRoom, waitForApp } from "./example-room";

// Under each bubble: reaction chips then the add-reaction smiley at the
// bottom-left; the time and the reply/edit/delete buttons pinned to the
// bottom-right. Uses `data-testid`.

async function selectRoom(page: Page, roomName: string) {
  const listed = page.getByTestId("room-list").getByRole("button", { name: roomName });

  if (!(await listed.isVisible({ timeout: 500 }).catch(() => false))) {
    const hamburger = page.locator('[data-testid="hamburger-rooms-button"]:visible');
    if (await hamburger.isVisible({ timeout: 500 }).catch(() => false)) {
      await hamburger.click();
    } else {
      const vp = page.viewportSize();
      if (vp && vp.width < 768) {
        await page.setViewportSize({ width: 1280, height: vp.height });
        await selectListedRoom(page, roomName);
        await page.setViewportSize({ width: vp.width, height: vp.height });
        // `--chat-col` comes from a ResizeObserver, so it is stale until the
        // next frame: let it catch up before anything measures the column.
        await page.evaluate(
          () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))
        );
        return;
      }
    }
  }

  await selectListedRoom(page, roomName);
}

function ownRow(page: Page): Locator {
  return page.locator('[id^="msg-"]:has(.bg-accent)').first();
}

function receivedRow(page: Page): Locator {
  return page.locator('[id^="msg-"]:not(:has(.bg-accent))').first();
}

const PLUS = '[data-testid="add-reaction-button"]';
const REPLY = '[data-testid="message-reply-button"]';
const EDIT = '[data-testid="message-edit-button"]';
const DEL = '[data-testid="message-delete-button"]';
const CHIP = '[data-testid="reaction-chip"]';
const COUNT = '[data-testid="reaction-chip-count"]';
const PICKER = '[data-testid="emoji-picker"]';
const TIME = '[data-testid="message-time"]';
const REACTION_ROW = '[data-testid="message-reaction-row"]';
const CLUSTER = '[data-testid="message-action-cluster"]';

// The bubble is the only element in a row carrying `msg-bubble`.
function bubbleOf(row: Locator): Locator {
  return row.locator(".msg-bubble").first();
}

function receivedWithReactions(page: Page): Locator {
  return page
    .locator(`[id^="msg-"]:not(:has(.bg-accent)):has(${CHIP})`)
    .first();
}

function withoutReactions(page: Page): Locator {
  return page.locator(`[id^="msg-"]:not(:has(${CHIP}))`).first();
}

// Self's message that example data gives a full row of reactions, all from
// OTHER members (so every chip there starts un-pressed for the viewer).
function outOfLineRow(page: Page): Locator {
  return page.locator('[id^="msg-"]', { hasText: "That was out of line." }).first();
}

const chipEmojis = (row: Locator) =>
  row.locator(CHIP).evaluateAll((els) => els.map((el) => el.getAttribute("data-emoji")));

const css = (loc: Locator, prop: string) =>
  loc.evaluate((el, p) => (getComputedStyle(el) as any)[p] as string, prop);

const opacity = (loc: Locator) => loc.evaluate((el) => parseFloat(getComputedStyle(el).opacity));

// `--color-row-hover` as a computed `background-color`, comparable with `css(row, "backgroundColor")`.
const rowHover = (page: Page) =>
  page.evaluate(() => {
    const p = document.createElement("div");
    p.style.backgroundColor = "var(--color-row-hover)";
    document.body.appendChild(p);
    const c = getComputedStyle(p).backgroundColor;
    p.remove();
    return c;
  });

const isCoarse = (page: Page) =>
  page.evaluate(() => window.matchMedia("(hover: none), (any-pointer: coarse)").matches);

// Each of `sels` inside `row` is hidden at rest and shown once `target` is hovered.
async function expectHiddenUntilHovered(page: Page, row: Locator, target: Locator, sels: string[]) {
  await row.scrollIntoViewIfNeeded();
  await page.mouse.move(0, 0);
  for (const sel of sels) await expect.poll(() => opacity(row.locator(sel)), sel).toBe(0);
  await target.hover();
  for (const sel of sels) await expect.poll(() => opacity(row.locator(sel)), sel).toBeGreaterThan(0);
}

// Every rule the reaction row's layout keeps, checked over every row: one
// message per broken rule, so `[]` means the layout is right.
const reactionRowProblems = (page: Page) =>
  page.evaluate(() => {
    const box = (e: Element) => e.getBoundingClientRect();
    const mid = (e: Element) => (box(e).top + box(e).bottom) / 2;
    const col = document.getElementById("chat-content")!;
    const cs = getComputedStyle(col);
    const column = col.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
    const problems: string[] = [];
    let checked = 0;
    let textOnly = 0;
    for (const row of document.querySelectorAll('[id^="msg-"]')) {
      const bubble = row.querySelector(".msg-bubble");
      const line = row.querySelector('[data-testid="message-reaction-row"]');
      const smiley = row.querySelector('[data-testid="add-reaction-button"]');
      const cluster = row.querySelector('[data-testid="message-action-cluster"]');
      if (!bubble || !line || !smiley || !cluster) continue;
      checked++;
      const chips = row.querySelectorAll('[data-testid="reaction-chip"]');
      const first = chips[0] ?? smiley;
      const last = chips[chips.length - 1];
      const fail = (rule: string) => problems.push(`${rule}: ${(row.textContent ?? "").trim().slice(0, 40)}`);

      if (Math.abs(box(first).left - box(bubble).left) > 1) fail("first chip or smiley not at the bubble's left edge");
      if (last && box(smiley).left < box(last).right - 0.5 && box(smiley).top < box(last).bottom - 0.5)
        fail("smiley not after the last chip");
      // A row of chips wider than the bubble's text cap widens the bubble, so
      // the buttons never overhang it.
      if (Math.abs(box(cluster).right - box(bubble).right) > 1) fail("buttons not flush with the bubble's right edge");
      // One line whenever the row's max-content width fits the column (WebKit's
      // emoji chips are ~3px wider than Chromium's, which once made only Safari wrap).
      const clone = line.cloneNode(true) as HTMLElement;
      clone.style.cssText = "position:absolute;visibility:hidden;width:max-content";
      line.parentElement!.appendChild(clone);
      const fits = clone.getBoundingClientRect().width <= column;
      clone.remove();
      if (fits && (Math.abs(mid(first) - mid(smiley)) > 2 || Math.abs(mid(first) - mid(cluster)) > 2))
        fail("wrapped though it fits the column");
      if (chips.length === 0) {
        textOnly++;
        if (box(bubble).width > column * 0.75 + 1) fail("text-only bubble wider than 75% of the column");
      }
    }
    if (checked === 0 || textOnly === 0) problems.push(`only ${checked} rows, ${textOnly} text-only`);
    return problems;
  });

const DEEP_HISTORY_BAND =
  "a row's band pads 8px around its group and 2px between rows, and bands touch";

test.beforeEach(async ({ page }, testInfo) => {
  // That case boots the capped-history fixture itself. Loading Your Private
  // Room first would throw the session away.
  if (testInfo.title === DEEP_HISTORY_BAND) return;
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Your Private Room");
});

test("action buttons: own rows reply, edit and delete; received rows reply and react", async ({ page }) => {
  const own = ownRow(page);
  const received = receivedRow(page);
  await expect(own.locator(REPLY)).toHaveAttribute("aria-label", "Reply");
  await expect(own.locator(EDIT)).toHaveAttribute("aria-label", "Edit message");
  await expect(own.locator(DEL)).toHaveAttribute("aria-label", "Delete message");
  await expect(received.locator(REPLY)).toHaveAttribute("aria-label", "Reply");
  await expect(received.locator(PLUS)).toHaveAttribute("aria-label", "Add reaction");
  await expect(received.locator(EDIT)).toHaveCount(0);
  await expect(received.locator(DEL)).toHaveCount(0);
});

// The widths are re-checked after a resize because the 75% text cap follows
// the column (`--chat-col` from `#chat-content`'s `onresize`), not the width
// the page loaded with.
test("reaction row layout: chips and smiley at the bubble's left, buttons at its right, one line when it fits", async ({
  page,
}) => {
  for (const width of [1280, 900]) {
    await page.setViewportSize({ width, height: 900 });
    await expect.poll(() => reactionRowProblems(page), { message: `at ${width}px` }).toEqual([]);
  }
});

// The cluster (time first, then the buttons) keeps 16px clear of what precedes
// it, which is now the smiley after the last chip.
test("a full reaction row keeps 16px between the smiley and the time and buttons", async ({ page }) => {
  const row = outOfLineRow(page);
  await row.scrollIntoViewIfNeeded();
  const smiley = (await row.locator(PLUS).boundingBox())!;
  const time = (await row.locator(TIME).boundingBox())!;
  // Only meaningful when the row did not wrap between them.
  test.skip(Math.abs(smiley.y + smiley.height / 2 - (time.y + time.height / 2)) > 4, "row wrapped");
  expect(time.x - (smiley.x + smiley.width)).toBeGreaterThanOrEqual(15);
});

test("a received message shows its time in the button cluster, before the reply arrow", async ({ page }) => {
  const row = receivedRow(page);
  await row.scrollIntoViewIfNeeded();
  await expect(row.locator(`${CLUSTER} ${TIME}`)).toHaveCount(1);
  await expect(row.locator(TIME)).toHaveText(/\d/);
  await expect(row.locator(TIME)).toHaveAttribute("title", /\d/);
  const time = (await row.locator(TIME).boundingBox())!;
  const reply = (await row.locator(REPLY).boundingBox())!;
  expect(time.x + time.width).toBeLessThanOrEqual(reply.x);
});

test("a reaction-less row is no taller than its time-and-buttons cluster", async ({ page }) => {
  const row = withoutReactions(page);
  await row.scrollIntoViewIfNeeded();
  const line = (await row.locator(REACTION_ROW).boundingBox())!;
  const cluster = (await row.locator(CLUSTER).boundingBox())!;
  expect(Math.abs(line.height - cluster.height)).toBeLessThanOrEqual(1);
});

test.describe("reaction chips", () => {
  test("clicking a chip someone else made adds your reaction, and again removes it", async ({
    page,
  }) => {
    const row = outOfLineRow(page);
    await row.scrollIntoViewIfNeeded();
    const first = row.locator(CHIP).first();
    const emoji = (await first.getAttribute("data-emoji"))!;
    await expect(first).toHaveAttribute("aria-pressed", "false");
    const before = Number(await first.locator(COUNT).textContent());
    const chip = row.locator(`${CHIP}[data-emoji="${emoji}"]`);
    const theirs = row.locator(`${CHIP}[aria-pressed="false"]`).first();

    await chip.click();
    await expect(chip).toHaveAttribute("aria-pressed", "true");
    await expect(chip.locator(COUNT)).toHaveText(String(before + 1));
    // Your own reaction's chip is marked by its fill. Polled: the click left
    // the pointer on the chip, and the hover colour transitions out.
    await page.mouse.move(0, 0);
    await expect
      .poll(async () => (await css(chip, "backgroundColor")) !== (await css(theirs, "backgroundColor")))
      .toBe(true);

    await chip.click();
    await expect(chip).toHaveAttribute("aria-pressed", "false");
    await expect(chip.locator(COUNT)).toHaveText(String(before));
  });

  test("a new emoji is appended after the existing chips", async ({ page }) => {
    const row = outOfLineRow(page);
    await row.scrollIntoViewIfNeeded();
    const before = await chipEmojis(row);
    await row.hover();
    await row.locator(PLUS).click();
    const options = (await page.locator(PICKER).locator("button").allTextContents()).map((s) => s.trim());
    const idx = options.findIndex((e) => !before.includes(e));
    expect(idx, "the picker offers an emoji not already on the message").toBeGreaterThanOrEqual(0);
    await page.locator(PICKER).locator("button").nth(idx).click();

    await expect(row.locator(CHIP)).toHaveCount(before.length + 1);
    expect(await chipEmojis(row)).toEqual([...before, options[idx]]);
    await expect(row.locator(CHIP).last()).toHaveAttribute("aria-pressed", "true");
    await expect(row.locator(CHIP).last().locator(COUNT)).toHaveText("1");
  });

  test("hovering a chip changes its background without resizing it", async ({ page }) => {
    test.skip(await isCoarse(page), "hover styles need a hover-capable pointer");

    const row = outOfLineRow(page);
    await row.scrollIntoViewIfNeeded();
    await page.mouse.move(0, 0);
    const chip = row.locator(`${CHIP}[aria-pressed="false"]`).first();
    const restBg = await css(chip, "backgroundColor");
    const rest = (await chip.boundingBox())!;

    await chip.hover();
    await expect.poll(() => css(chip, "backgroundColor")).not.toBe(restBg);
    // The border is always there, invisible at rest, so hover shifts nothing.
    const hovered = (await chip.boundingBox())!;
    expect(Math.abs(hovered.width - rest.width)).toBeLessThanOrEqual(0.5);
    expect(Math.abs(hovered.height - rest.height)).toBeLessThanOrEqual(0.5);
  });

  // A bubble's corners follow only its place in its group. A reaction used to
  // square off a bottom corner of a group's LAST bubble (the only position
  // whose shape it changed), so react to one of those and compare all four.
  for (const kind of ["own", "received"] as const) {
    test(`adding a reaction leaves the bubble's corners unchanged (${kind})`, async ({ page }) => {
      const side = kind === "own" ? ":has(.bg-accent)" : ":not(:has(.bg-accent))";
      const candidate = page
        .locator(`.msg-bubbles > .msg-row:last-child${side}:not(:has(${CHIP}))`)
        .last();
      // Pinned by id: the `:not(:has(chip))` locator stops matching once it reacts.
      const row = page.locator(`[id="${await candidate.getAttribute("id")}"]`);
      const corners = () =>
        bubbleOf(row).evaluate((el) => {
          const s = getComputedStyle(el);
          return [
            s.borderTopLeftRadius,
            s.borderTopRightRadius,
            s.borderBottomRightRadius,
            s.borderBottomLeftRadius,
          ];
        });
      await row.scrollIntoViewIfNeeded();
      const before = await corners();

      await row.hover();
      await row.locator(PLUS).click();
      await page.locator(PICKER).locator("button").first().click();
      await expect(row.locator(CHIP)).toHaveCount(1);

      expect(await corners()).toEqual(before);
    });
  }
});

// Narrowest supported width.
test.describe("picker at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  for (const kind of ["own", "received"] as const) {
    test(`via smiley (${kind})`, async ({ page }) => {
      const row = kind === "own" ? ownRow(page) : receivedRow(page);
      await row.scrollIntoViewIfNeeded();
      await row.hover();
      await row.locator(PLUS).click();
      const picker = (await page.locator(PICKER).boundingBox())!;
      expect(picker.x).toBeGreaterThanOrEqual(-1);
      expect(picker.x + picker.width).toBeLessThanOrEqual(321);
    });
  }
});

test("clicking delete asks for confirmation", async ({ page }) => {
  const row = ownRow(page);
  await row.hover();
  await row.locator(DEL).click();
  const cancel = page.getByRole("button", { name: "Cancel" });
  await expect(cancel).toBeVisible();
  await cancel.click();
  await expect(row.locator(EDIT)).toHaveCount(1);
});

test("clicking the reply arrow opens the composer reply preview", async ({
  page,
}) => {
  const row = receivedRow(page);
  await row.hover();
  await row.locator(REPLY).click();
  await expect(page.getByTitle("Cancel reply")).toBeVisible({
    timeout: 5_000,
  });
});

// The time and buttons show on hover with a mouse, and always on touch.
test.describe("hover reveals", () => {
  test("a message's smiley, reply arrow and time stay hidden until it is hovered", async ({ page }) => {
    const coarse = await isCoarse(page);
    for (const row of [receivedWithReactions(page), ownRow(page)]) {
      if (coarse) {
        // No hover on touch, so they must be visible at rest.
        for (const sel of [PLUS, REPLY, TIME])
          await expect.poll(() => opacity(row.locator(sel)), sel).toBeGreaterThanOrEqual(0.4);
      } else {
        await expectHiddenUntilHovered(page, row, bubbleOf(row), [PLUS, REPLY, TIME]);
      }
    }
  });

  test("hovering anywhere on a message row fills the whole row with the band grey", async ({ page }) => {
    test.skip(await isCoarse(page), "no hover band on touch");
    const row = ownRow(page); // our own, right-aligned: the row's left part is empty
    await row.scrollIntoViewIfNeeded();
    const band = await rowHover(page);
    await page.mouse.move(0, 0);
    await expect.poll(() => css(row, "backgroundColor")).toBe("rgba(0, 0, 0, 0)");

    // The row spans the message column plus 8px each side, not just its bubble,
    // and the bubble stays at the column's edge.
    const r = (await row.boundingBox())!;
    const col = (await page.getByTestId("conversation-history").boundingBox())!;
    expect(Math.abs(r.x - (col.x - 8))).toBeLessThanOrEqual(1);
    expect(Math.abs(r.x + r.width - (col.x + col.width + 8))).toBeLessThanOrEqual(1);

    // Hovering the empty space left of the bubble fills the row and reveals its buttons.
    const b = (await bubbleOf(row).boundingBox())!;
    expect(Math.abs(b.x + b.width - (col.x + col.width))).toBeLessThanOrEqual(1);
    expect(b.x - r.x).toBeGreaterThan(40);
    await page.mouse.move(r.x + 10, r.y + r.height / 2);
    await expect.poll(() => css(row, "backgroundColor")).toBe(band);
    await expect.poll(() => opacity(row.locator(REPLY))).toBeGreaterThan(0);
  });

  // The space around a row is padding inside its band (main.css `.msg-row`):
  // 8px at the sides and at a group's top and bottom, 2px between rows of one
  // group. So neighbouring bands touch and hovering never finds a dead strip.
  // The capped fixture room's authors come in pairs, so it always has two-row
  // groups back to back.
  test(DEEP_HISTORY_BAND, async ({ page }) => {
    await page.goto("/?deep-history-room=1");
    await waitForApp(page);
    await selectRoom(page, "Capped History Room");
    await expect(page.locator(".msg-bubbles > .msg-row:nth-child(2)").first()).toBeAttached();
    const problems = await page.evaluate(() => {
      const box = (e: Element) => e.getBoundingClientRect();
      const history = document.querySelector('[data-testid="conversation-history"]')!;
      const col = box(history);
      const problems: string[] = [];
      let prevLast: Element | null = null;
      let multiRow = 0;
      let adjacent = 0;
      for (const item of history.children) {
        const rows = Array.from(item.querySelectorAll(".msg-bubbles > .msg-row"));
        if (rows.length === 0) {
          prevLast = null; // a date separator or event row sits between
          continue;
        }
        if (rows.length > 1) multiRow++;
        rows.forEach((row, i) => {
          const r = box(row);
          const fail = (rule: string) => problems.push(`${rule}: ${(row.textContent ?? "").trim().slice(0, 30)}`);
          if (Math.abs(r.left - (col.left - 8)) > 1 || Math.abs(r.right - (col.right + 8)) > 1)
            fail("band not 8px past the column's sides");
          const above = box(row.firstElementChild!).top - r.top;
          const below = r.bottom - box(row.lastElementChild!).bottom;
          if (Math.abs(above - (i === 0 ? 8 : 2)) > 0.5) fail(`${above}px above the content`);
          if (Math.abs(below - (i === rows.length - 1 ? 8 : 2)) > 0.5) fail(`${below}px below the content`);
        });
        // Within the group, and from the previous group when nothing sits between.
        const stack = prevLast ? [prevLast, ...rows] : rows;
        for (let i = 1; i < stack.length; i++)
          if (Math.abs(box(stack[i]).top - box(stack[i - 1]).bottom) > 0.5) problems.push("gap between two bands");
        if (prevLast) adjacent++;
        prevLast = rows[rows.length - 1];
      }
      if (multiRow === 0 || adjacent === 0)
        problems.push(`only ${multiRow} multi-row groups, ${adjacent} back-to-back groups`);
      return problems;
    });
    expect(problems).toEqual([]);
  });

  test("hovering a group's author header fills its first row and reveals that message's controls", async ({
    page,
  }) => {
    test.skip(await isCoarse(page), "no hover band on touch; controls stay visible");
    const first = page.locator('[id^="msg-"]:has(.msg-group-header)').first();
    const header = first.locator(".msg-group-header");
    await expectHiddenUntilHovered(page, first, header, [PLUS, REPLY, TIME]);
    await expect.poll(() => css(first, "backgroundColor")).toBe(await rowHover(page));
    const r = (await first.boundingBox())!;
    const h = (await header.boundingBox())!;
    expect(h.y).toBeGreaterThanOrEqual(r.y - 0.5);
    expect(h.y + h.height).toBeLessThanOrEqual(r.y + r.height + 0.5);
    // Later rows of the group carry no header of their own.
    await expect(page.locator(".msg-row:not(:first-child) .msg-group-header")).toHaveCount(0);
  });

  test("the smiley and the row band stay while its picker is open", async ({ page }) => {
    test.skip(await isCoarse(page), "buttons are always visible on touch");
    const row = receivedWithReactions(page);
    await row.scrollIntoViewIfNeeded();
    await bubbleOf(row).hover();
    await row.locator(PLUS).click();
    await expect(page.locator(PICKER)).toBeVisible();
    // No class keeps them shown: the picker's full-screen backdrop sits inside
    // the row, so the row stays hovered wherever the mouse goes.
    await page.mouse.move(0, 0);
    await expect.poll(() => opacity(row.locator(PLUS))).toBe(1);
    await expect.poll(() => css(row, "backgroundColor")).toBe(await rowHover(page));
  });
});
