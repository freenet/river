import { test, expect } from "@playwright/test";

// `.msg-react-btn` / `.msg-action-btn` (ui/assets/main.css) reveal on `:hover`,
// which a touch pointer can't trigger, so without the touch rule they render at
// opacity 0 yet still take taps (same failure as #402 and #462). Probed against
// the shipped stylesheets. Also pins that there is NO minimum tap size: one was
// tried in #605 and reverted because it made every reaction row taller.

test("on a touch pointer the smiley and action buttons are visible at rest, with no minimum tap size", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  const coarse = await page.evaluate(() => window.matchMedia("(hover: none), (any-pointer: coarse)").matches);
  test.skip(!coarse, "touch only; message-reply-button's hover reveals cover a mouse");

  for (const className of ["msg-react-btn", "msg-action-btn"]) {
    // Mirrors the real markup in `conversation.rs`: a button inside a `group` row.
    const m = await page.evaluate((className) => {
      const row = document.createElement("div");
      row.className = "group relative";
      const btn = document.createElement("button");
      btn.className = className;
      row.appendChild(btn);
      document.body.appendChild(row);
      const { width, height } = btn.getBoundingClientRect();
      const opacity = parseFloat(getComputedStyle(btn).opacity);
      row.remove();
      return { opacity, size: Math.min(width, height) };
    }, className);

    expect(m.opacity, `${className} is invisible on touch: main.css's touch rule regressed`).toBeGreaterThanOrEqual(0.4);
    // A plain <button> renders well under 44px, so this catches a re-added min size.
    expect(m.size, `${className} has a minimum tap size again (reverted in #605)`).toBeLessThan(44);
  }
});
