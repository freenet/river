import { test, expect, Locator, Page } from "@playwright/test";
import { coarsePointer, contrast, openInlinePicker, openRoomWithComposer, reactTo, resolveColor, rowSurfaces, waitForApp } from "./example-room";
import { deliverOffscreen, readerLeavesTheEnd, readerScrollsWithoutGesture, scrollMetrics } from "./history-geometry";
import { callRiverTest } from "./river-test";

// Full-width chat rows (plan 11): every author's group gets one River-logo
// avatar and one header in its first row, every row shares one content column,
// and the right 72px of each row is the gutter the hover bar and the touch
// kebab sit in.

const GUTTER_PX = 72;

/// The lowest-contrast (hue, tone) per theme, from `worst_case` in
/// avatar_colour.rs (light 3.29:1, dark 6.06:1); update both together.
const WORST_AVATAR = { light: [191, 2], dark: [352, 0] } as const;

/// The history item (a group of one author's rows) holding `text`.
function groupWith(page: Page, text: string): Locator {
  return page.locator("[data-item-key]").filter({ hasText: text });
}

async function openRoom(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
}

/// A received pair (one fixed test identity, inserted before the newest
/// message) and an own pair (two sends from the composer). Example data picks
/// its authors at random, so neither pair is left to chance.
async function openRoomWithPairs(page: Page): Promise<{ received: Locator; own: Locator }> {
  await openRoom(page);
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

type Box = { left: number; right: number; top: number; bottom: number };
const width = (b: Box) => b.right - b.left;
type RowGeometry = {
  self: string | null;
  row: Box;
  contentLeft: number;
  avatars: Array<Box & { color: string; background: string; ariaHidden: string | null; tabIndex: number }>;
  headers: Box[];
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
      return {
        self: row.getAttribute("data-message-self"),
        row: box(row),
        contentLeft: row.getBoundingClientRect().left + parseFloat(cs.paddingLeft),
        avatars: Array.from(row.querySelectorAll('[data-testid="message-group-avatar"]')).map((a) => ({
          ...box(a),
          color: getComputedStyle(a).color,
          background: getComputedStyle(a).backgroundColor,
          ariaHidden: a.getAttribute("aria-hidden"),
          tabIndex: (a as HTMLElement).tabIndex,
        })),
        headers: Array.from(row.querySelectorAll('[data-testid="message-group-header"]')).map(box),
        body: box(row.querySelector('[data-testid="message-body"]')!),
        hoverBar: shown(row.querySelector(".hover-actions")),
        kebab: shown(row.querySelector('[data-testid="message-kebab"]')),
      };
    });
  });
}

/// A received group's avatar (plan 11b): its hue and tone are inline, and its
/// painted colour is chromatic (not grey) and at least 3:1 against the
/// conversation background. Returns the painted colour.
async function expectHashedAvatar(page: Page, group: Locator, label: string): Promise<number[]> {
  const avatar = group.getByTestId("message-group-avatar");
  const { hue, tone, color } = await avatar.evaluate((a) => ({
    hue: (a as HTMLElement).style.getPropertyValue("--avatar-hue").trim(),
    tone: (a as HTMLElement).style.getPropertyValue("--avatar-tone").trim(),
    color: getComputedStyle(a).color,
  }));
  expect(hue, `${label}: inline --avatar-hue`).toMatch(/^\d+$/);
  expect(tone, `${label}: inline --avatar-tone`).toMatch(/^\d+$/);
  const painted = await resolveColor(page, color);
  const [r, g, b] = painted;
  expect(Math.max(r, g, b) - Math.min(r, g, b), `${label}: chromatic, not grey (${painted})`).toBeGreaterThanOrEqual(40);
  const bg = await resolveColor(page, "var(--color-bg)");
  expect(contrast(painted, bg), `${label}: 3:1 against the background (${painted} on ${bg})`).toBeGreaterThanOrEqual(3);
  return painted;
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
      expect(avatar.left, `${kind}: avatar at the row's start`).toBeCloseTo(first.contentLeft, 0);
      // The header starts the content column, and the continuation row keeps it.
      expect(first.headers[0].left).toBeCloseTo(first.body.left, 0);
      expect(second.body.left, `${kind}: continuation aligned`).toBeCloseTo(first.body.left, 0);
      expect(avatar.top, `${kind}: avatar beside its header`).toBeCloseTo(first.headers[0].top, 0);

      for (const r of rows) {
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
    }
    // Same left edge for both authors: no right-aligned own group.
    expect(Math.max(...bodyLefts) - Math.min(...bodyLefts)).toBeLessThanOrEqual(0.5);

    // The own avatar is set apart: a disc where a received avatar has none.
    const receivedAvatar = groups.received[0].avatars[0];
    expect(groups.own[0].avatars[0].background, "own avatar looks different").not.toBe(receivedAvatar.background);

    // The colour comes from the member id, so the same author gets it again after a reload.
    const painted = await resolveColor(page, receivedAvatar.color);
    await page.reload();
    await waitForApp(page);
    await openRoomWithComposer(page);
    await callRiverTest(page, "insertMessageBeforeLast", "received after reload");
    const again = await rowGeometry(groupWith(page, "received after reload"));
    expect(await resolveColor(page, again[0].avatars[0].color)).toEqual(painted);
  });

  test("received avatars take each member id's own colour, clear of the background in both themes", async ({ page }) => {
    await openRoom(page);
    // Three fixed identities: Test Sender Insert, then appended arrivals
    // alternate between Test Sender A and B.
    await callRiverTest(page, "insertMessageBeforeLast", "hashed colour insert");
    for (const text of ["hashed colour arrival one", "hashed colour arrival two"]) {
      await deliverOffscreen(page, text);
    }
    const groups = {
      insert: groupWith(page, "hashed colour insert"),
      first: groupWith(page, "hashed colour arrival one"),
      second: groupWith(page, "hashed colour arrival two"),
    };
    await expect(groups.insert).toBeAttached();
    const names = await Promise.all(
      Object.values(groups).map((g) => g.getByTestId("message-group-header").getByTestId("message-author-name").getAttribute("title")),
    );
    expect(new Set(names).size, `three distinct members: ${names}`).toBe(3);

    for (const scheme of ["light", "dark"] as const) {
      await page.emulateMedia({ colorScheme: scheme });
      const colours: string[] = [];
      for (const [kind, g] of Object.entries(groups)) {
        colours.push(String(await expectHashedAvatar(page, g, `${scheme} ${kind}`)));
      }
      expect(new Set(colours).size, `${scheme}: distinct members, distinct colours (${colours.join(" | ")})`).toBe(3);
    }
  });

  test("the worst-case avatar colour clears 3:1 over the hover band, and tone moves it", async ({ page }) => {
    const { received } = await openRoomWithPairs(page);
    const avatar = received.getByTestId("message-group-avatar");
    const paint = async (hue: number, tone: number) =>
      resolveColor(
        page,
        await avatar.evaluate((a, [h, t]) => {
          (a as HTMLElement).style.setProperty("--avatar-hue", String(h));
          (a as HTMLElement).style.setProperty("--avatar-tone", String(t));
          return getComputedStyle(a).color;
        }, [hue, tone]),
      );
    for (const scheme of ["light", "dark"] as const) {
      await page.emulateMedia({ colorScheme: scheme });
      const [hue, tone] = WORST_AVATAR[scheme];
      const { band } = await rowSurfaces(page);
      const worst = await paint(hue, tone);
      expect(contrast(worst, band), `${scheme}: hue ${hue} tone ${tone} on the band (${worst} on ${band})`).toBeGreaterThanOrEqual(3);
      expect(await paint(hue, (tone + 1) % 3), `${scheme}: tone changes the colour`).not.toEqual(worst);
    }
  });

  test("a group's name is a button that opens the member's info from the keyboard", async ({ page }) => {
    const { received } = await openRoomWithPairs(page);
    const name = received.getByTestId("message-group-header").getByRole("button", { name: "Test Sender Insert" });
    await name.focus();
    await expect(name).toBeFocused();
    await page.keyboard.press("Enter");
    const modal = page.getByTestId("member-info-modal");
    await expect(modal).toBeVisible();
    await expect(modal).toContainText("Test Sender Insert");
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
// in the gutter and on screen. Regression (#212): a self reply whose
// reply-strip preview held a long unbreakable URL rendered nowrap and spilled
// off both edges of a 375px screen. The example data carries a self reply to
// the long-URL message so this reproduces.
test.describe("Message rows at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("nothing overflows the column and the controls stay reachable", async ({ page }) => {
    const { own } = await openRoomWithPairs(page);
    // The example room's fixtures: a long unbreakable link and a reply quoting it.
    await expect(page.getByTestId("message-body").filter({ hasText: "longlongurlpath" }).first()).toBeAttached();
    await expect(page.locator(".reply-strip").filter({ hasText: "longlongurl" }).first()).toBeAttached();
    // A send drops the fixture reactions, so react to the newest own message.
    await reactTo(page, own.getByTestId("message-row").last());

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

// The inline reaction picker hangs off the "+" at the end of a row's reaction
// chips, and the history scroller clips anything past its sides. At 320px a
// few chips push "+" far enough right that a rightward picker loses its
// right-hand emoji, so the picker opens toward the side of the scroller with
// more room. Each case checks every emoji is inside the scroller and is the
// element a tap at its centre lands on.
type PickerReport = {
  scroller: Box;
  anchor: Box;
  picker: Box;
  buttons: Array<{ emoji: string; inside: boolean; hit: boolean }>;
};

/// Open `row`'s inline picker from its "+" and measure it.
async function openPickerFromPlus(page: Page, row: Locator): Promise<PickerReport> {
  await openInlinePicker(page, row);
  return measurePicker(page);
}

/// The open picker, its "+" wrapper (the picker's containing block) and the
/// scroller, with no click in between: a click elsewhere would close it.
async function measurePicker(page: Page): Promise<PickerReport> {
  await expect(page.getByTestId("emoji-picker")).toBeVisible();
  return page.evaluate(() => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    const scroller = box(document.getElementById("chat-scroll-container")!);
    const picker = document.querySelector('[data-testid="emoji-picker"]')!;
    const plus = picker.parentElement!.querySelector('[data-testid="add-reaction-button"]')!;
    const buttons = Array.from(picker.querySelectorAll("button")).map((b) => {
      const r = b.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {
        emoji: (b.textContent || "").trim(),
        inside:
          r.left >= scroller.left - 0.5 &&
          r.right <= scroller.right + 0.5 &&
          r.top >= scroller.top - 0.5 &&
          r.bottom <= scroller.bottom + 0.5,
        hit: at !== null && (at === b || b.contains(at)),
      };
    });
    return {
      scroller,
      anchor: box(plus.parentElement!),
      picker: box(picker),
      buttons,
    };
  });
}

function expectEveryEmojiTappable(report: PickerReport) {
  expect(report.buttons.length, "the picker has emoji").toBeGreaterThan(0);
  const outside = report.buttons.find((b) => !b.inside);
  expect(outside, `${outside?.emoji} is clipped by the scroller (picker ${report.picker.left}..${report.picker.right}, scroller ${report.scroller.left}..${report.scroller.right})`).toBeUndefined();
  const covered = report.buttons.find((b) => !b.hit);
  expect(covered, `${covered?.emoji} is not the element a tap at its centre lands on`).toBeUndefined();
}

/// The newest message, "picker edge", with as many reaction chips as leave its
/// "+" on their line, so "+" ends near the column's right edge. A fixed count
/// does not do it: four chips left "+" at 160px in Chromium, where a 140px
/// picker still fit, and six wrapped "+" to the line's start in WebKit, whose
/// emoji chips are 3px wider.
async function nearEdgeRow(page: Page): Promise<Locator> {
  const emojis = ["👍", "❤️", "😂", "🔥", "🎉", "👀", "😮", "😢", "🙏", "💯"];
  await openRoom(page);
  await deliverOffscreen(page, "picker edge");
  const row = page.getByTestId("message-row").filter({ hasText: "picker edge" });
  const chips = row.getByTestId("reaction-chip");
  await callRiverTest(page, "addReactionsToNewest", emojis.slice(0, 1));
  await expect(chips).toHaveCount(1);
  const fit = await row.evaluate((el) => {
    const chip = el.querySelector('[data-testid="reaction-chip"]')!.getBoundingClientRect();
    const plus = el.querySelector('[data-testid="add-reaction-button"]')!.getBoundingClientRect();
    const line = el.querySelector('[data-testid="message-reaction-row"]')!.getBoundingClientRect();
    // Each chip moves "+" one chip and its gap further right.
    return Math.floor((line.right - plus.right) / (plus.left - chip.left)) + 1;
  });
  expect(fit, "chips that fit one line").toBeLessThanOrEqual(emojis.length);
  await callRiverTest(page, "addReactionsToNewest", emojis.slice(1, fit));
  await expect(chips).toHaveCount(fit);
  await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
  return row;
}

/// What `left-0` would do: a rightward picker from this "+" ends past the scroller.
function expectRightwardWouldClip(report: PickerReport) {
  expect(
    report.anchor.left + width(report.picker),
    `a full line of chips no longer pushes + past the point where a rightward picker clips (+ at ${report.anchor.left}, picker ${width(report.picker)} wide, scroller ends at ${report.scroller.right})`,
  ).toBeGreaterThan(report.scroller.right);
}

test.describe("Reaction picker at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("near the right edge, the picker opens leftward and every emoji is tappable", async ({ page }) => {
    const row = await nearEdgeRow(page);
    const report = await openPickerFromPlus(page, row);
    expectRightwardWouldClip(report);
    expectEveryEmojiTappable(report);
  });

  test("at the start of the line, the picker opens rightward", async ({ page }) => {
    await openRoom(page);
    await deliverOffscreen(page, "picker start");
    const row = page.getByTestId("message-row").filter({ hasText: "picker start" });
    await expect(row.getByTestId("reaction-chip")).toHaveCount(0);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    const report = await openPickerFromPlus(page, row);
    // What an unconditional `right-0` would do: a leftward picker from this "+" starts before the scroller.
    expect(
      report.anchor.right - width(report.picker),
      `+ no longer starts the line (+ ends at ${report.anchor.right}, picker ${width(report.picker)} wide)`,
    ).toBeLessThan(report.scroller.left);
    expectEveryEmojiTappable(report);
  });

  test("the kebab's React opens the picker on the same side", async ({ page }) => {
    test.skip(!(await coarsePointer(page)), "the kebab shows only on a coarse pointer");
    const row = await nearEdgeRow(page);
    await row.getByTestId("message-kebab").click();
    // The menu fills in a tick after it opens.
    const react = page.getByTestId("message-action-menu").getByRole("button", { name: "React" });
    await expect(react).toBeVisible();
    await react.click();
    const report = await measurePicker(page);
    expectRightwardWouldClip(report);
    expectEveryEmojiTappable(report);
  });
});

// The hover band (fine pointers only): hovering anywhere in a message row,
// avatar included, fills that row and only that row; it stays while a control
// the row opened is expanded.
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
  test("only the hovered row lights, from its avatar or its body", async ({ page }) => {
    test.skip(await coarsePointer(page), "no hover band on a touch pointer");
    const { received } = await openRoomWithPairs(page);
    const [first, second] = [received.getByTestId("message-row").first(), received.getByTestId("message-row").nth(1)];
    const band = await resolveColor(page, "var(--color-row-hover)");
    expect(band[3], "premise: the band token resolves to a visible colour").toBeGreaterThan(0);

    for (const [what, target, row] of [
      ["the avatar", received.getByTestId("message-group-avatar"), first],
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
  });

  test.describe("at a narrow width", () => {
    test.use({ viewport: { width: 480, height: 800 } });

    test("the band stays while the row's reactor list is open, and removing the reaction does not scroll", async ({ page }) => {
      test.skip(await coarsePointer(page), "no hover band on a touch pointer");
      const { received } = await openRoomWithPairs(page);
      const row = received.getByTestId("message-row").nth(1);
      await row.scrollIntoViewIfNeeded();
      const chip = await reactTo(page, row);

      const band = await resolveColor(page, "var(--color-row-hover)");
      // Park the reader short of the end, where nothing but a scroll moves
      // scrollTop: at the end, a shorter reaction row would clamp it.
      await readerScrollsWithoutGesture(page, (await scrollMetrics(page)).max - 150);
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

// A row's resting controls (the hover bar on a hover pointer, the kebab on
// touch) sit in the gutter, which can share Latest's corner. They paint under
// Latest, so a row at the bottom of the view never takes Latest's click.
test.describe("Latest over row controls", () => {
  test("a row control aligned with Latest never takes its click", async ({ page }) => {
    const touch = await coarsePointer(page);
    await page.setViewportSize(touch ? { width: 390, height: 800 } : { width: 1024, height: 800 });
    await openRoom(page);
    const control = touch ? ".touch-actions" : ".hover-actions";
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
    if (!touch) await row.locator(".msg-content").hover({ position: { x: 4, y: 4 } });
    // Premise: the control really overlaps Latest, or the test proves nothing.
    const [c, l] = [(await row.locator(control).boundingBox())!, (await latest.boundingBox())!];
    const [left, right] = [Math.max(c.x, l.x), Math.min(c.x + c.width, l.x + l.width)];
    const [top, bottom] = [Math.max(c.y, l.y), Math.min(c.y + c.height, l.y + l.height)];
    expect(right - left, "premise: the control overlaps Latest across").toBeGreaterThan(0);
    expect(bottom - top, "premise: the control overlaps Latest down").toBeGreaterThan(0);
    const onTop = await latest.evaluate(
      (el, [x, y]) => el.contains(document.elementFromPoint(x, y)),
      [(left + right) / 2, (top + bottom) / 2],
    );
    expect(onTop, "Latest is the top element where they overlap").toBe(true);
    await latest.click();
    await expect(latest).toBeHidden();
  });
});
