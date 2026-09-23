import { test, expect, Page } from "@playwright/test";
import { selectListedRoom, waitForApp } from "./example-room";

// Clicking a reply's quote strip (`data-testid="reply-strip"`) scrolls to the
// quoted message and fills its row with the bubble grey for 2s, switching on
// and off at once, with no fade (`conversation/reply_highlight.rs`,
// `.msg-row.reply-highlight` in main.css).
// The row is the band's only container, the same one the hover band uses: it
// spans the message column and, for a group's first message, holds the author
// name too.
//
// Fixture: in "Public Discussion Room" the local user is an observer, so every
// group has a name header, and the first reply quotes a group's first row.

const ROOM = "Public Discussion Room";

async function selectRoom(page: Page, roomName: string) {
  const listed = page.getByTestId("room-list").getByRole("button", { name: roomName });
  if (!(await listed.isVisible({ timeout: 500 }).catch(() => false))) {
    // Narrow-window case: temporarily expand to click the room.
    const vp = page.viewportSize();
    if (vp && vp.width < 768) {
      await page.setViewportSize({ width: 1280, height: vp.height });
      await selectListedRoom(page, roomName);
      await page.setViewportSize({ width: vp.width, height: vp.height });
      // `--chat-col` (the bubble width cap) is published from a
      // ResizeObserver, so it lags the resize by a frame.
      await page.evaluate(
        () =>
          new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))
      );
      return;
    }
  }
  await selectListedRoom(page, roomName);
}

async function openRoom(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, ROOM);
  await expect(page.getByTestId("reply-strip").first()).toBeVisible({
    timeout: 10_000,
  });
}

type Rect = { left: number; top: number; right: number; bottom: number };

type BandInfo = {
  /** The highlighted element is the quoted row itself. */
  hostIsRow: boolean;
  band: Rect;
  /** The history list: exactly the message column. */
  column: Rect;
  scroller: Rect;
  /** The author name in the target's group header, if the group has one. */
  name: Rect | null;
  /** The row after the target in its group, if any. */
  nextRow: Rect | null;
  targetIsFirstRow: boolean;
};

/** Where the band is: the highlighted element's own box. */
async function readBand(page: Page, targetId: string): Promise<BandInfo> {
  return page.evaluate((id) => {
    const plain = (r: DOMRect) => ({
      left: r.left,
      top: r.top,
      right: r.right,
      bottom: r.bottom,
    });
    const hosts = document.querySelectorAll(".reply-highlight");
    if (hosts.length !== 1) {
      throw new Error(`expected one highlighted element, found ${hosts.length}`);
    }
    const host = hosts[0] as HTMLElement;
    const row = document.getElementById(id);
    if (!row) throw new Error(`no row ${id}`);
    const name = row.querySelector('.msg-group-header [title^="Member ID:"]');
    const next = row.nextElementSibling;
    return {
      hostIsRow: host === row,
      band: plain(host.getBoundingClientRect()),
      column: plain(
        document
          .querySelector('[data-testid="conversation-history"]')!
          .getBoundingClientRect()
      ),
      scroller: plain(
        document.getElementById("chat-scroll-container")!.getBoundingClientRect()
      ),
      name: name ? plain(name.getBoundingClientRect()) : null,
      nextRow: next ? plain(next.getBoundingClientRect()) : null,
      targetIsFirstRow: row.previousElementSibling === null,
    };
  }, targetId);
}

/** `currentTime` of the live highlight animation, or null if none. */
async function highlightTime(page: Page): Promise<number | null> {
  return page.evaluate(() => {
    const anim = document
      .getAnimations()
      .find(
        (a) =>
          (a as CSSAnimation).animationName === "replyHighlight" &&
          a.playState !== "idle"
      );
    if (!anim) return null;
    return Number(anim.currentTime ?? 0);
  });
}

type Sample = {
  /** `currentTime` of the row's highlight animation (ms), or null when none. */
  animTime: number | null;
  /** The row still carries the highlight class. */
  on: boolean;
  /** The row's computed background, as sRGB bytes. */
  bg: number[];
  /** A `background-color` transition is running on the row. */
  fading: boolean;
};

/**
 * Samples the row's background once per frame, from just before `trigger`
 * runs until the highlight class has been off for 20 frames.
 */
async function recordRow(
  page: Page,
  rowId: string,
  trigger: () => Promise<void>
): Promise<Sample[]> {
  await page.evaluate((id) => {
    const row = document.getElementById(id);
    if (!row) throw new Error(`no row ${id}`);
    const w = window as unknown as { __rowSamples: unknown[]; __rowDone: boolean };
    w.__rowSamples = [];
    w.__rowDone = false;
    const cv = document.createElement("canvas");
    cv.width = cv.height = 1;
    const ctx = cv.getContext("2d", { willReadFrequently: true })!;
    const bytes = (c: string) => {
      ctx.clearRect(0, 0, 1, 1);
      ctx.fillStyle = c;
      ctx.fillRect(0, 0, 1, 1);
      return Array.from(ctx.getImageData(0, 0, 1, 1).data);
    };
    const started = performance.now();
    let seenOn = false;
    let offFrames = 0;
    const tick = () => {
      const on = row.classList.contains("reply-highlight");
      const anims = row.getAnimations();
      const hl = anims.find(
        (a) => (a as CSSAnimation).animationName === "replyHighlight"
      );
      w.__rowSamples.push({
        animTime: hl && hl.currentTime !== null ? Number(hl.currentTime) : null,
        on,
        bg: bytes(getComputedStyle(row).backgroundColor),
        fading: anims.some(
          (a) => (a as CSSTransition).transitionProperty === "background-color"
        ),
      });
      if (on) seenOn = true;
      else if (seenOn) offFrames++;
      if (offFrames >= 20 || performance.now() - started > 8000) {
        w.__rowDone = true;
        return;
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }, rowId);
  await trigger();
  await page.waitForFunction(
    () => (window as unknown as { __rowDone: boolean }).__rowDone,
    null,
    { timeout: 10_000 }
  );
  return page.evaluate(
    () => (window as unknown as { __rowSamples: Sample[] }).__rowSamples
  );
}

/** A CSS colour (e.g. `var(--color-surface)`) resolved on the page, as sRGB bytes. */
async function colourBytes(page: Page, value: string): Promise<number[]> {
  return page.evaluate((value) => {
    const p = document.createElement("div");
    p.style.backgroundColor = value;
    document.body.appendChild(p);
    const c = getComputedStyle(p).backgroundColor;
    p.remove();
    const cv = document.createElement("canvas");
    cv.width = cv.height = 1;
    const ctx = cv.getContext("2d")!;
    ctx.fillStyle = c;
    ctx.fillRect(0, 0, 1, 1);
    return Array.from(ctx.getImageData(0, 0, 1, 1).data);
  }, value);
}

const nearBytes = (a: number[], b: number[]) =>
  a.every((v, i) => Math.abs(v - b[i]) <= 4);

/**
 * From 1s into the highlight: full strength until 2s, then exactly `base`
 * (the row's own background) with nothing in between, and never a
 * `background-color` transition, including after `animationend` takes the
 * class off. Starting at 1s leaves out the hover test's pointer move.
 */
function expectInstantOff(samples: Sample[], surface: number[], base: number[]) {
  const start = samples.findIndex((s) => s.animTime !== null);
  expect(start, "the highlight animation ran").toBeGreaterThanOrEqual(0);
  expect(
    samples.findIndex((s, i) => i > start && s.animTime === null),
    "the highlight animation ended"
  ).toBeGreaterThan(start);
  expect(samples[samples.length - 1].on, "the class came off").toBe(false);

  let lastHeld = -1;
  samples.forEach((s, i) => {
    const t = s.animTime;
    if (i < start || (t !== null && t < 1000)) return;
    const where = `frame ${i} (${t === null ? "after the animation" : `at ${Math.round(t)}ms`})`;
    expect(s.fading, `${where}: no background-color transition`).toBe(false);
    if (t !== null && t < 1990) {
      expect(nearBytes(s.bg, surface), `${where}: full strength, got ${s.bg}`).toBe(true);
      lastHeld = Math.max(lastHeld, t);
    } else if (t === null || t >= 2010) {
      expect(nearBytes(s.bg, base), `${where}: the row's own background, got ${s.bg}`).toBe(true);
    } else {
      expect(
        nearBytes(s.bg, surface) || nearBytes(s.bg, base),
        `${where}: on or off, never in between, got ${s.bg}`
      ).toBe(true);
    }
  });
  expect(lastHeld, "still full strength shortly before 2s").toBeGreaterThanOrEqual(1700);
}

async function targetOf(page: Page): Promise<string> {
  const id = await page
    .getByTestId("reply-strip")
    .first()
    .getAttribute("data-reply-target");
  expect(id, "reply strip names the row it jumps to").toBeTruthy();
  return id!;
}

// The band reaches 8px past the column on each side (main.css `.msg-row`).
function expectSpansColumn(info: BandInfo) {
  expect(Math.abs(info.band.left - (info.column.left - 8))).toBeLessThanOrEqual(2);
  expect(Math.abs(info.band.right - (info.column.right + 8))).toBeLessThanOrEqual(2);
}

function contains(outer: Rect, inner: Rect, slack = 0.5) {
  return (
    outer.left <= inner.left + slack &&
    outer.right >= inner.right - slack &&
    outer.top <= inner.top + slack &&
    outer.bottom >= inner.bottom - slack
  );
}

test.describe("Reply-jump highlight", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("is still on at 1.5s and gone within 4s", async ({ page }) => {
    await openRoom(page);
    const highlight = page.locator(".reply-highlight");
    await page.getByTestId("reply-strip").first().click();
    const t0 = Date.now();
    await expect(highlight).toHaveCount(1);

    await page.waitForTimeout(Math.max(0, 1500 - (Date.now() - t0)));
    expect(await highlight.count()).toBe(1);

    // The class comes off on `animationend` (2.1s: the 2s hold plus a 0.1s
    // tail that already shows the row's own background).
    await expect(highlight).toHaveCount(0, {
      timeout: Math.max(0, 4000 - (Date.now() - t0)),
    });
  });

  test("holds in the bubble grey at full opacity", async ({ page }) => {
    await openRoom(page);
    await page.getByTestId("reply-strip").first().click();
    await expect(page.locator(".reply-highlight")).toHaveCount(1);
    await page.waitForTimeout(1000); // mid-hold (the hold lasts 2s)
    // Compared as sRGB bytes: an animated colour and a probe can serialise the
    // same colour differently (`color(srgb …)`, `oklab(…)`, `rgba(…)`).
    const [band, want] = await page.evaluate(() => {
      const bytes = (c: string) => {
        const cv = document.createElement("canvas");
        cv.width = cv.height = 1;
        const ctx = cv.getContext("2d")!;
        ctx.clearRect(0, 0, 1, 1);
        ctx.fillStyle = c;
        ctx.fillRect(0, 0, 1, 1);
        return Array.from(ctx.getImageData(0, 0, 1, 1).data);
      };
      const p = document.createElement("div");
      p.style.backgroundColor = "var(--color-surface)";
      document.body.appendChild(p);
      const want = getComputedStyle(p).backgroundColor;
      p.remove();
      const host = document.querySelector(".reply-highlight")!;
      return [bytes(getComputedStyle(host).backgroundColor), bytes(want)];
    });
    expect(band[3], "fully opaque").toBe(255);
    for (let i = 0; i < 4; i++) expect(Math.abs(band[i] - want[i])).toBeLessThanOrEqual(3);
  });

  // Keyboard activation leaves the pointer where it is, parked off the
  // conversation, so the row's own background is its unhovered one.
  test("switches off at 2s with no fade", async ({ page }) => {
    await openRoom(page);
    const targetId = await targetOf(page);
    await page.mouse.move(0, 0);
    const strip = page.getByTestId("reply-strip").first();
    const surface = await colourBytes(page, "var(--color-surface)");
    const rest = await page.evaluate(
      (id) => getComputedStyle(document.getElementById(id)!).backgroundColor,
      targetId
    );
    const base = await colourBytes(page, rest);

    const samples = await recordRow(page, targetId, async () => {
      await strip.focus();
      await page.keyboard.press("Enter");
    });
    expectInstantOff(samples, surface, base);
  });

  // The row's own background is the hover band when the highlight ends under
  // the pointer, which is what the animation's tail must already show.
  test("switches off at 2s with no fade while the row is hovered", async ({
    page,
  }) => {
    await openRoom(page);
    test.skip(
      !(await page.evaluate(() => window.matchMedia("(hover: hover)").matches)),
      "the hover band needs a hover-capable pointer"
    );
    const targetId = await targetOf(page);
    await page.mouse.move(0, 0);
    const strip = page.getByTestId("reply-strip").first();
    const surface = await colourBytes(page, "var(--color-surface)");
    const hover = await colourBytes(page, "var(--color-row-hover)");

    const samples = await recordRow(page, targetId, async () => {
      await strip.focus();
      await page.keyboard.press("Enter");
      await expect(page.locator(".reply-highlight")).toHaveCount(1);
      const box = (await page.locator(`[id="${targetId}"]`).boundingBox())!;
      await page.mouse.move(
        box.x + box.width / 2,
        box.y + Math.min(box.height / 2, 24)
      );
    });
    expect(
      nearBytes(samples[samples.length - 1].bg, hover),
      "the pointer is on the row once the highlight is off"
    ).toBe(true);
    expectInstantOff(samples, surface, hover);
  });

  test("includes the author name when the quote is a group's first message", async ({
    page,
  }) => {
    await openRoom(page);
    const targetId = await targetOf(page);
    await page.getByTestId("reply-strip").first().click();
    await expect(page.locator(".reply-highlight")).toHaveCount(1);

    const info = await readBand(page, targetId);
    // The same container the hover band fills.
    expect(info.hostIsRow).toBe(true);
    // Fixture precondition (see the header comment).
    expect(info.targetIsFirstRow).toBe(true);
    expect(info.name, "target group has an author name header").not.toBeNull();

    expect(contains(info.band, info.name!)).toBe(true);
    expectSpansColumn(info);
    // The band stops at the quoted row rather than covering the whole group.
    if (info.nextRow) {
      expect(info.band.bottom).toBeLessThanOrEqual(info.nextRow.top + 0.5);
    }
    // The jump scrolls the name into view, not just the row.
    expect(info.name!.top).toBeGreaterThanOrEqual(info.scroller.top - 0.5);
    expect(info.name!.bottom).toBeLessThanOrEqual(info.scroller.bottom + 0.5);
  });

  test("activating the same strip again restarts the highlight", async ({
    page,
  }) => {
    await openRoom(page);
    const strip = page.getByTestId("reply-strip").first();

    // Mid-highlight: the animation starts over rather than running on.
    await strip.click();
    await expect(page.locator(".reply-highlight")).toHaveCount(1);
    await page.waitForTimeout(1000);
    expect(await highlightTime(page)).toBeGreaterThan(700);
    await strip.click();
    await expect(page.locator(".reply-highlight")).toHaveCount(1);
    const restarted = await highlightTime(page);
    expect(restarted).not.toBeNull();
    expect(restarted!).toBeLessThan(500);

    // After it has finished: the class is gone, and a new click brings it back.
    await expect(page.locator(".reply-highlight")).toHaveCount(0, {
      timeout: 4_000,
    });
    await strip.click();
    await expect(page.locator(".reply-highlight")).toHaveCount(1);
  });
});

test.describe("Reply-jump highlight at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("the band stays inside the viewport, with no page overflow", async ({
    page,
  }) => {
    await openRoom(page);
    const targetId = await targetOf(page);
    await page.getByTestId("reply-strip").first().click();
    await expect(page.locator(".reply-highlight")).toHaveCount(1);

    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - window.innerWidth
    );
    expect(overflow).toBeLessThanOrEqual(0);
    const info = await readBand(page, targetId);
    expect(info.band.left).toBeGreaterThanOrEqual(0);
    expect(info.band.right).toBeLessThanOrEqual(320);
  });
});
