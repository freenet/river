import { test, expect } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

// Regression coverage for the inline add-reaction "+" button being invisible
// (but still tappable) on touch devices.
//
// `.add-reaction-btn` (ui/assets/main.css) reveals via `.group:hover`, a real
// CSS `:hover`, which touch devices can't reliably trigger — so on a phone
// the button rendered at `opacity: 0` yet still received taps (the reported
// symptom: invisible, but tapping roughly where it should be opens the
// picker). Same failure mode as freenet/river#402 and #462; the fix mirrors
// #462's pointer-capability media query. This probes the cascade directly
// against the shipped stylesheets, since that's the layer where the bug
// actually lived (Tailwind's utility layer vs. main.css's plain rules).
//
// Deliberately does NOT assert a minimum tap-target size: an earlier version
// of this fix added `min-width`/`min-height: 2.75rem` (mirroring #462), but
// that grew the reaction row's height on every message and read as a visible
// regression (river chat feedback, 2026-08-07) — reverted. The size
// assertion below pins that decision: it fails if `min-width`/`min-height`
// gets re-added to `.add-reaction-btn` without deliberately revisiting this.

const PROBE_ID = "add-reaction-cascade-probe";

async function measureProbe(page: import("@playwright/test").Page, hasReactions: boolean) {
  return page.evaluate(
    ({ id, hasReactions }) => {
      // Mirrors the real markup in `conversation.rs`: an add-reaction button
      // inside a `group` row, revealed by `.group:hover`.
      const row = document.createElement("div");
      row.className = "group relative";
      const btn = document.createElement("button");
      btn.id = id;
      btn.className =
        "add-reaction-btn" + (hasReactions ? " has-reactions" : "");
      row.appendChild(btn);
      document.body.appendChild(row);

      const style = getComputedStyle(btn);
      const rect = btn.getBoundingClientRect();
      const result = {
        coarse: window.matchMedia("(hover: none), (any-pointer: coarse)").matches,
        opacity: style.opacity,
        width: rect.width,
        height: rect.height,
      };
      row.remove();
      return result;
    },
    { id: PROBE_ID, hasReactions }
  );
}

test.describe("Add-reaction + button cascade", () => {
  for (const hasReactions of [false, true]) {
    test(`touch reveal beats the hover-only rule, and only on a coarse pointer (has-reactions=${hasReactions})`, async ({
      page,
    }) => {
      await page.goto("/");
      await waitForApp(page);

      const m = await measureProbe(page, hasReactions);

      if (m.coarse) {
        // The bug: `.group:hover .add-reaction-btn` can never match without a
        // hover pointer, so without the touch rule this stays at opacity 0
        // (or 0.2 with existing reactions) — dim-to-invisible, but still
        // tappable underneath.
        expect(
          parseFloat(m.opacity),
          "on a coarse pointer `.add-reaction-btn` must be clearly visible " +
            "at rest — if this is 0 (or 0.2), main.css's touch rule regressed " +
            "and the + button is invisible again"
        ).toBeGreaterThanOrEqual(0.4);
        // No min-width/min-height on touch — see the file header. A plain
        // <button> with no explicit sizing renders well under 44px from UA
        // default padding alone, so this catches a re-added min-size rule.
        expect(
          Math.min(m.width, m.height),
          "the button must NOT have a forced minimum tap-target size on " +
            "touch — that was tried and reverted because it grew the " +
            "reaction row's height on every message (visible regression)"
        ).toBeLessThan(44);
      } else {
        // Mouse-only: the hover reveal must be preserved.
        const expected = hasReactions ? 0.2 : 0;
        expect(
          parseFloat(m.opacity),
          "on a mouse-only pointer the button must stay at its hover-gated " +
            "rest opacity"
        ).toBeCloseTo(expected, 5);
      }
    });
  }
});

test("on touch, the + takes taps 20px around it without growing the row", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
  test.skip(
    !(await page.evaluate(() => matchMedia("(hover: none), (any-pointer: coarse)").matches)),
    "the hit area is touch-only"
  );

  // A short reaction row (one or two chips), centred in the history so every probe lands on the page.
  // Last in its group: the next row of a group starts right below, and its bubble paints over the hit area.
  const LAST = '[id^="msg-"]:last-child';
  const CHIP = '[data-testid="reaction-chip"]';
  let id = await page.evaluate(
    ([last, chip]) =>
      [...document.querySelectorAll(last)].find((r) => {
        const n = r.querySelectorAll(chip).length;
        return n > 0 && n <= 2;
      })?.id ?? null,
    [LAST, CHIP]
  );
  if (!id) {
    // The random history shows none: react to one through the picker first.
    id = await page.locator(`${LAST}:not(:has(${CHIP}))`).last().evaluate((el) => el.id);
    await page.locator(`[id="${id}"]`).getByTestId("add-reaction-button").click();
    await page.getByTestId("emoji-picker").locator("button").first().click();
    await expect(page.locator(`[id="${id}"]`).locator(CHIP)).toHaveCount(1);
  }
  const row = page.locator(`[id="${id}"]`);
  await row.evaluate((el) => {
    const scroller = document.getElementById("chat-scroll-container")!;
    const plus = el.querySelector('[data-testid="add-reaction-button"]')!.getBoundingClientRect();
    const s = scroller.getBoundingClientRect();
    scroller.scrollBy(0, plus.top - (s.top + s.height / 2));
  });
  const m = await row.evaluate((el) => {
    const plus = el.querySelector('[data-testid="add-reaction-button"]')!;
    const chips = el.querySelectorAll('[data-testid="reaction-chip"]');
    const chip = chips[chips.length - 1];
    const r = plus.getBoundingClientRect();
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    const hitsPlus = (x: number, y: number) =>
      document.elementFromPoint(x, y)?.closest('[data-testid="add-reaction-button"]') === plus;
    const c = chip.getBoundingClientRect();
    return {
      height: r.height,
      above: hitsPlus(cx, cy - 20),
      below: hitsPlus(cx, cy + 20),
      right: hitsPlus(cx + 20, cy),
      chip:
        document.elementFromPoint(c.left + c.width / 2, c.top + c.height / 2)?.closest('[data-testid="reaction-chip"]') ===
        chip,
    };
  });
  // The box itself did not grow (#605): only the hit area did.
  expect(m.height).toBeLessThan(32);
  expect(m.above, "a tap 20px above the + misses it").toBe(true);
  expect(m.below, "a tap 20px below the + misses it").toBe(true);
  expect(m.right, "a tap 20px right of the + misses it").toBe(true);
  expect(m.chip, "the hit area steals the chip's taps").toBe(true);
});
