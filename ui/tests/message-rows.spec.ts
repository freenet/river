import { test, expect, Locator, Page } from "@playwright/test";
import { openRoomWithComposer, resolveColor, settle, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// Full-width chat rows (plan 11): every author's group gets one River-logo
// avatar and one header in its first row, every row shares one content column,
// and the right 72px of each row is the gutter the hover bar and the touch
// kebab sit in.

const GUTTER_PX = 72;

/// A received pair (one fixed test identity, inserted before the newest
/// message) and an own pair (two sends from the composer). Example data picks
/// its authors at random, so neither pair is left to chance.
async function openRoomWithPairs(page: Page): Promise<{ received: Locator; own: Locator }> {
  await page.goto("/");
  await waitForApp(page);
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
  const group = (text: string) => page.locator("[data-item-key]").filter({ hasText: text });
  const received = group("received pair one");
  const own = group("own pair one");
  await expect(received).toContainText("received pair two");
  await expect(own).toContainText("own pair two");
  return { received, own };
}

type Box = { left: number; right: number; top: number; bottom: number };
type RowGeometry = {
  self: string | null;
  row: Box;
  contentLeft: number;
  avatars: Array<Box & { color: string; background: string; radius: string; ariaHidden: string | null; tabIndex: number; cursor: string }>;
  headers: Box[];
  nameColor: string | null;
  body: Box;
  hoverBar: Box | null;
  kebab: Box | null;
};

function rowGeometry(group: Locator): Promise<RowGeometry[]> {
  return group.evaluate((el) => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    // Laid out at all: `display: none` (the kebab on a fine pointer) reports no boxes.
    const shown = (e: Element | null) => (e && e.getClientRects().length > 0 ? box(e) : null);
    return Array.from(el.querySelectorAll('[data-testid="message-row"]')).map((row) => {
      const cs = getComputedStyle(row);
      const name = row.querySelector('[data-testid="message-group-header"] span[title^="Member ID"]');
      return {
        self: row.getAttribute("data-message-self"),
        row: box(row),
        contentLeft: row.getBoundingClientRect().left + parseFloat(cs.paddingLeft),
        avatars: Array.from(row.querySelectorAll('[data-testid="message-group-avatar"]')).map((a) => ({
          ...box(a),
          color: getComputedStyle(a).color,
          background: getComputedStyle(a).backgroundColor,
          radius: getComputedStyle(a).borderRadius,
          ariaHidden: a.getAttribute("aria-hidden"),
          tabIndex: (a as HTMLElement).tabIndex,
          cursor: getComputedStyle(a).cursor,
        })),
        headers: Array.from(row.querySelectorAll('[data-testid="message-group-header"]')).map(box),
        nameColor: name ? getComputedStyle(name).color : null,
        body: box(row.querySelector('[data-testid="message-body"]')!),
        hoverBar: shown(row.querySelector(".hover-actions")),
        kebab: shown(row.querySelector('[data-testid="message-kebab"]')),
      };
    });
  });
}

function computedColor(page: Page, color: string) {
  return resolveColor(page, color);
}

test.describe("Message rows", () => {
  test("own and received groups share one row layout, with one avatar and header per group", async ({ page }) => {
    const { received, own } = await openRoomWithPairs(page);
    const groups = { received: await rowGeometry(received), own: await rowGeometry(own) };

    const bodyLefts: number[] = [];
    for (const [kind, rows] of Object.entries(groups)) {
      expect(rows, `${kind}: a two-message group`).toHaveLength(2);
      expect(rows.map((r) => r.self)).toEqual(Array(2).fill(kind === "own" ? "true" : "false"));
      // One avatar and one header, both in the first row.
      expect(rows.map((r) => r.avatars.length), `${kind}: avatars per row`).toEqual([1, 0]);
      expect(rows.map((r) => r.headers.length), `${kind}: headers per row`).toEqual([1, 0]);

      const [first, second] = rows;
      const avatar = first.avatars[0];
      expect(avatar.right - avatar.left).toBeCloseTo(36, 0);
      expect(avatar.left, `${kind}: avatar at the row's start`).toBeCloseTo(first.contentLeft, 0);
      expect(first.body.left - avatar.right, `${kind}: 12px between avatar and content`).toBeCloseTo(12, 0);
      // The header starts the content column, and the continuation row keeps it.
      expect(first.headers[0].left).toBeCloseTo(first.body.left, 0);
      expect(second.body.left, `${kind}: continuation aligned`).toBeCloseTo(first.body.left, 0);
      expect(avatar.top, `${kind}: avatar beside its header`).toBeCloseTo(first.headers[0].top, 0);

      for (const r of rows) {
        // Full available width: the content column ends where the gutter starts.
        expect(r.body.right, `${kind}: content ends at the gutter`).toBeCloseTo(r.row.right - GUTTER_PX, 0);
        bodyLefts.push(r.body.left);
        // The hover bar and kebab sit inside the gutter, for both authors.
        for (const [control, b] of [["hover bar", r.hoverBar], ["kebab", r.kebab]] as const) {
          if (!b) continue;
          expect(b.left, `${kind}: ${control} starts in the gutter`).toBeGreaterThanOrEqual(r.body.right - 0.5);
          expect(b.right, `${kind}: ${control} ends inside the row`).toBeLessThanOrEqual(r.row.right + 0.5);
        }
      }
      expect(first.hoverBar ?? first.kebab, `${kind}: a control is laid out`).not.toBeNull();

      // Decorative but clickable: the name stays the accessible control.
      expect(avatar.ariaHidden).toBe("true");
      expect(avatar.tabIndex).toBe(-1);
      expect(avatar.cursor).toBe("pointer");
    }
    // Same left edge for both authors: no right-aligned own group.
    expect(Math.max(...bodyLefts) - Math.min(...bodyLefts)).toBeLessThanOrEqual(0.5);

    const text = await computedColor(page, "var(--color-text)");
    const accent = await computedColor(page, "var(--color-accent)");
    const ownFirst = groups.own[0];
    const receivedFirst = groups.received[0];
    expect(await computedColor(page, ownFirst.nameColor!), "own name in accent").toEqual(accent);
    const bg = await computedColor(page, "var(--color-bg)");
    const ownAvatar = ownFirst.avatars[0];
    expect(await computedColor(page, ownAvatar.background), "own avatar: text-coloured disc").toEqual(text);
    expect(await computedColor(page, ownAvatar.color), "own avatar: glyph in the background colour").toEqual(bg);
    expect(ownAvatar.radius, "own avatar is round").toBe("9999px");
    expect(receivedFirst.avatars[0].background, "received avatar has no disc").toBe("rgba(0, 0, 0, 0)");
    expect(await computedColor(page, receivedFirst.nameColor!), "received name unchanged").toEqual(text);
    const receivedAvatar = await computedColor(page, receivedFirst.avatars[0].color);
    expect(receivedAvatar, "received avatar from the palette").not.toEqual(text);
    expect(receivedAvatar).not.toEqual(accent);

    // The colour comes from the member id, so the same author gets it again after a reload.
    await page.reload();
    await waitForApp(page);
    await openRoomWithComposer(page);
    await callRiverTest(page, "insertMessageBeforeLast", "received after reload");
    const again = await rowGeometry(page.locator("[data-item-key]").filter({ hasText: "received after reload" }));
    expect(await computedColor(page, again[0].avatars[0].color)).toEqual(receivedAvatar);
  });

  test("clicking a group's avatar opens that member's info, like the name", async ({ page }) => {
    const { received } = await openRoomWithPairs(page);
    const avatar = received.getByTestId("message-group-avatar");
    await avatar.scrollIntoViewIfNeeded();
    await avatar.click();
    const modal = page.getByTestId("member-info-modal");
    await expect(modal).toBeVisible();
    await expect(modal).toContainText("Test Sender Insert");
  });
});

// The narrowest supported width: a long message, a long link, a reply and a
// reaction row must wrap inside the content column, and the controls must stay
// in the gutter and on screen.
test.describe("Message rows at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("nothing overflows the column and the controls stay reachable", async ({ page }) => {
    const { own } = await openRoomWithPairs(page);
    // The example room's fixtures: a long unbreakable link and a reply quoting it.
    await expect(page.getByTestId("message-body").filter({ hasText: "longlongurlpath" }).first()).toBeAttached();
    await expect(page.locator(".reply-strip").filter({ hasText: "longlongurl" }).first()).toBeAttached();
    // A send drops the fixture reactions, so react to the newest own message.
    const newest = own.getByTestId("message-row").last();
    if (!(await page.evaluate(() => matchMedia("(hover: none), (any-pointer: coarse)").matches))) {
      await newest.hover();
    }
    await newest.getByTestId("add-reaction-button").click();
    await page.getByTestId("emoji-picker").getByRole("button").first().click();
    await expect(newest.getByTestId("reaction-chip")).toHaveCount(1);

    const report = await page.evaluate((gutter) => {
      const vw = document.documentElement.clientWidth;
      const scroller = document.getElementById("chat-scroll-container")!;
      const sr = scroller.getBoundingClientRect();
      const bad: string[] = [];
      const label = (e: Element) => (e.textContent || "").trim().replace(/\s+/g, " ").slice(0, 30);
      for (const row of Array.from(document.querySelectorAll('[data-testid="message-row"]'))) {
        const r = row.getBoundingClientRect();
        if (r.left < sr.left - 0.5 || r.right > sr.right + 0.5) bad.push(`row ${label(row)}: ${r.left}..${r.right}`);
        const column = r.right - gutter;
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
    }, GUTTER_PX);
    expect(report.bad).toEqual([]);
    expect(report.docOverflow).toBeLessThanOrEqual(0);
    expect(report.scrollerOverflow).toBeLessThanOrEqual(0);

    // Controls on the newest own message: in the gutter, inside the scroller,
    // and the touch kebab is the element a tap there lands on.
    const row = own.getByTestId("message-row").last();
    const [geometry] = (await rowGeometry(own)).slice(-1);
    const scroller = (await page.locator("#chat-scroll-container").boundingBox())!;
    for (const b of [geometry.hoverBar, geometry.kebab]) {
      if (!b) continue;
      expect(b.left).toBeGreaterThanOrEqual(geometry.body.right - 0.5);
      expect(b.right).toBeLessThanOrEqual(scroller.x + scroller.width + 0.5);
    }
    if (geometry.kebab) {
      const kebab = row.getByTestId("message-kebab");
      const hit = await kebab.evaluate((el) => {
        const r = el.getBoundingClientRect();
        const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return at !== null && (at === el || el.contains(at));
      });
      expect(hit, "the kebab is hit-testable").toBe(true);
    }
  });
});

// The hover band (fine pointers only): hovering anywhere in a message row,
// avatar and header included, fills that row and only that row, bleeding 8px
// past the column on each side; it stays while a control the row opened is
// expanded.
async function fine(page: Page) {
  return !(await page.evaluate(() => matchMedia("(hover: none), (any-pointer: coarse)").matches));
}

function background(row: Locator) {
  return row.evaluate(async (el) => {
    // Let the band's fade settle: a poll would pass on a sample mid-fade.
    await Promise.all(el.getAnimations().map((a) => a.finished));
    return getComputedStyle(el).backgroundColor;
  });
}

async function bandedRows(page: Page): Promise<string[]> {
  return page.getByTestId("message-row").evaluateAll((rows) =>
    rows
      .filter((r) => {
        const c = getComputedStyle(r).backgroundColor;
        return c !== "rgba(0, 0, 0, 0)" && c !== "transparent";
      })
      .map((r) => r.id),
  );
}

test.describe("Message row hover band", () => {
  test("only the hovered row lights, across the column, with the avatar and header", async ({ page }) => {
    test.skip(!(await fine(page)), "no hover band on a touch pointer");
    const { received } = await openRoomWithPairs(page);
    const [first, second] = [received.getByTestId("message-row").first(), received.getByTestId("message-row").nth(1)];
    const band = await resolveColor(page, "var(--color-row-hover)");
    expect(band[3], "premise: the band token resolves to a visible colour").toBeGreaterThan(0);

    for (const [what, target, row] of [
      ["the avatar", received.getByTestId("message-group-avatar"), first],
      ["the header", received.getByTestId("message-group-header"), first],
      ["the first body", first.getByTestId("message-body"), first],
      ["a continuation row", second.getByTestId("message-body"), second],
    ] as const) {
      await page.mouse.move(0, 0);
      await expect.poll(() => bandedRows(page), { message: "idle: no band" }).toEqual([]);
      await target.hover();
      await expect
        .poll(async () => resolveColor(page, await background(row)), { message: `${what}: band` })
        .toEqual(band);
      expect(await bandedRows(page), `${what}: only its row`).toEqual([await row.getAttribute("id")]);
    }

    // Across the column: 8px past the history on each side, inside the scroller.
    const geometry = await first.evaluate((el) => {
      const r = el.getBoundingClientRect();
      const h = document.querySelector('[data-testid="conversation-history"]')!.getBoundingClientRect();
      const s = document.getElementById("chat-scroll-container")!;
      return { left: h.left - r.left, right: r.right - h.right, overflow: s.scrollWidth - s.clientWidth };
    });
    expect(geometry.left).toBeCloseTo(8, 0);
    expect(geometry.right).toBeCloseTo(8, 0);
    expect(geometry.overflow).toBeLessThanOrEqual(0);

    // 8px at the group's edges, 2px between its rows.
    const spacing = await received.evaluate((el) => {
      const [a, b] = Array.from(el.querySelectorAll('[data-testid="message-row"]'));
      const top = (e: Element) => e.getBoundingClientRect().top;
      return {
        groupTop: top(a.querySelector('[data-testid="message-group-avatar"]')!) - top(a),
        betweenRows: top(b.querySelector('[data-testid="message-body"]')!) - top(b),
        groupBottom: b.getBoundingClientRect().bottom - b.querySelector(".msg-content")!.getBoundingClientRect().bottom,
      };
    });
    expect(spacing.groupTop).toBeCloseTo(8, 0);
    expect(spacing.betweenRows).toBeCloseTo(2, 0);
    expect(spacing.groupBottom).toBeCloseTo(8, 0);
  });

  test.describe("at a narrow width", () => {
    test.use({ viewport: { width: 480, height: 800 } });

    test("the band stays while the row's reactor list is open, and closing it does not scroll", async ({ page }) => {
      test.skip(!(await fine(page)), "no hover band on a touch pointer");
      const { received } = await openRoomWithPairs(page);
      const row = received.getByTestId("message-row").nth(1);
      await row.scrollIntoViewIfNeeded();
      await row.hover();
      await row.getByTestId("add-reaction-button").click();
      await page.getByTestId("emoji-picker").getByRole("button").first().click();
      const chip = row.getByTestId("reaction-chip");
      await expect(chip).toHaveCount(1);

      const band = await resolveColor(page, "var(--color-row-hover)");
      const scrollTop = () => page.evaluate(() => document.getElementById("chat-scroll-container")!.scrollTop);
      // Park the reader short of the end, where nothing but a scroll moves
      // scrollTop: at the end, a shorter reaction row would clamp it.
      await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        c.scrollTop = c.scrollHeight - c.clientHeight - 150;
      });
      await settle(page);
      await expect(row).toBeInViewport();
      await chip.focus();
      await page.keyboard.press("Enter");
      const owners = page.getByTestId("reaction-owners");
      await expect(owners).toBeVisible();
      await expect(chip).toHaveAttribute("aria-expanded", "true");
      await page.mouse.move(0, 0);
      expect(await resolveColor(page, await background(row)), "the band stays while the list is open").toEqual(band);
      const box = (await owners.boundingBox())!;
      expect(box.x).toBeGreaterThanOrEqual(-0.5);
      expect(box.x + box.width).toBeLessThanOrEqual(480.5);

      const before = await scrollTop();
      await page.keyboard.press("Escape");
      await expect(owners).toHaveCount(0);
      await expect.poll(() => bandedRows(page), { message: "the band goes with the list" }).toEqual([]);
      expect(await scrollTop(), "closing the list does not scroll").toBe(before);

      // Remove the reaction from the list: the chip goes, and still nothing scrolls.
      await chip.focus();
      await page.keyboard.press("Enter");
      await page.getByTestId("reaction-remove").click();
      await expect(row.getByTestId("reaction-chip")).toHaveCount(0);
      await expect(owners).toHaveCount(0);
      expect(await scrollTop(), "removing the reaction does not scroll").toBe(before);
    });
  });
});
