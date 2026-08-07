import { test, expect } from "@playwright/test";

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
      await page.waitForSelector(".app-root", { timeout: 30_000 });

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
