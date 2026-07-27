import { test, expect } from "@playwright/test";

// Regression coverage for freenet/river#462: the DM rail's archive ✕ was
// unreachable on touch.
//
// The bug lived entirely in the CSS cascade, so that is what this measures —
// against the SHIPPED stylesheets, in a real engine, not against the source.
// The source-side pins in `dm_rail_section.rs` check that the markup still
// carries the hooks and that the rule still sits in a pointer-capability media
// query; neither of them can see the thing that actually broke, which is
// whether `.dm-archive-btn { opacity: 1 }` in `main.css` beats Tailwind's
// `opacity-0` utility in `styles.css` at run time. Two ways it silently would
// not: if `main.css` were ever pulled into an `@layer` (layered rules lose to
// unlayered ones, and Tailwind puts every utility in `@layer utilities`), or if
// the two `Stylesheet` tags in `app.rs` were reordered while the layering
// changed. Both are one-line edits that no source scrape would catch.
//
// The real rail cannot be driven end-to-end here: the example-data build ships
// no DMs, so no rail row renders (see `dm-archive-ux.spec.ts`). So this probes
// the cascade with a synthetic element carrying the same classes, which is the
// part that was broken.

const PROBE_ID = "dm-archive-cascade-probe";

async function measureProbe(page: import("@playwright/test").Page) {
  return page.evaluate((id) => {
    // Mirrors the real markup in `dm_rail_section.rs`: an `opacity-0` archive
    // button inside a `group` row, revealed by `group-hover`.
    const row = document.createElement("div");
    row.className = "group relative dm-rail-row-btn";
    const btn = document.createElement("button");
    btn.id = id;
    btn.className =
      "dm-archive-btn absolute right-1 opacity-0 group-hover:opacity-100";
    row.appendChild(btn);
    document.body.appendChild(row);

    const style = getComputedStyle(btn);
    const rect = btn.getBoundingClientRect();
    const result = {
      // Does this engine report a coarse pointer at all? Drives the
      // expectation, so the same spec is meaningful on every project.
      coarse: window.matchMedia("(hover: none), (any-pointer: coarse)").matches,
      opacity: style.opacity,
      width: rect.width,
      height: rect.height,
      rowPaddingRight: getComputedStyle(row).paddingRight,
    };
    row.remove();
    return result;
  }, PROBE_ID);
}

test.describe("DM archive ✕ cascade (#462)", () => {
  test("the touch rule beats Tailwind's opacity-0, and only on a coarse pointer", async ({
    page,
  }) => {
    await page.goto("/");
    await page.waitForSelector(".app-root", { timeout: 30_000 });

    const m = await measureProbe(page);

    if (m.coarse) {
      // The bug: on touch, Tailwind wraps `group-hover:*` in
      // `@media (hover: hover)`, so nothing could ever reveal the button.
      expect(
        m.opacity,
        "on a coarse pointer `.dm-archive-btn` must force full opacity — if " +
          "this is 0, main.css lost the cascade to Tailwind's opacity-0 " +
          "utility and the ✕ is invisible again (#462)"
      ).toBe("1");
      // WCAG 2.5.8 asks for 24px; Apple HIG for 44. The rule sets 2.75rem.
      expect(
        Math.min(m.width, m.height),
        "the touch tap target must be at least 44px"
      ).toBeGreaterThanOrEqual(44);
      expect(
        parseFloat(m.rowPaddingRight),
        "the row's right padding must widen on touch so the enlarged ✕ does " +
          "not overlap the nickname or unread badge"
      ).toBeGreaterThanOrEqual(56);
    } else {
      // Mouse-only: the hover reveal must be preserved, which means the
      // button really is invisible at rest. If this reads 1, the touch rule
      // escaped its media query and the ✕ is now permanently visible on
      // desktop.
      expect(
        m.opacity,
        "on a mouse-only pointer the ✕ must stay hidden until hover"
      ).toBe("0");
    }
  });

});
