import { test, expect } from "@playwright/test";
import { coarsePointer, colourOf, contrast, over, resolveColor, selectRoom, waitForApp } from "./example-room";

// Text in a message row sits on the conversation background, the row's hover
// band, or a mention chip's fill over either. Links, mentions and the own
// name share one link colour, and reply strips use the quote colour; all must
// clear WCAG AA (4.5:1) in both themes, at rest and on hover. Every surface is
// read from the live stylesheet, never copied into the test.

const AA_TEXT = 4.5;

for (const scheme of ["light", "dark"] as const) {
  test(`${scheme}: links, mentions and reply strips clear 4.5:1 at rest and on hover`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: scheme });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    const c = (css: string) => resolveColor(page, css);
    // A mention chip's own fill, from a probe span carrying the real classes.
    const fillOf = async (cls: string) =>
      c(
        await page.evaluate((cls) => {
          const s = Object.assign(document.createElement("span"), { className: cls });
          document.getElementById("chat-content")!.appendChild(s);
          const bg = getComputedStyle(s).backgroundColor;
          s.remove();
          return bg;
        }, cls),
      );

    const bg = await c("var(--color-bg)");
    const band = over(await c("var(--color-row-hover)"), bg);
    const surfaces: Record<string, number[]> = { bg, band };
    for (const cls of ["river-mention", "river-mention river-mention-self"]) {
      const fill = await fillOf(cls);
      surfaces[`${cls} on bg`] = over(fill, bg);
      surfaces[`${cls} on band`] = over(fill, band);
    }
    for (const token of ["--color-link", "--color-link-hover"]) {
      // An undefined token would resolve to the inherited text colour and pass.
      const declared = await page.evaluate((t) => getComputedStyle(document.documentElement).getPropertyValue(t).trim(), token);
      expect(declared, `premise: ${token} is declared`).not.toBe("");
      const fg = await c(`var(${token})`);
      for (const [name, s] of Object.entries(surfaces)) {
        expect(contrast(fg, s), `${token} on ${name}`).toBeGreaterThanOrEqual(AA_TEXT);
      }
    }

    // The markup paints with those tokens.
    const link = await c("var(--color-link)");
    const history = page.locator("#chat-scroll-container");
    for (const sel of [".prose a", ".river-mention", '[data-message-self="true"] span[title^="Member ID"]']) {
      const el = history.locator(sel).first();
      await expect(el, `premise: ${sel} is rendered`).toBeAttached();
      expect(await colourOf(el), sel).toEqual(link);
    }
    if (!(await coarsePointer(page))) {
      const a = history.locator(".prose a").first();
      await a.scrollIntoViewIfNeeded();
      await a.hover();
      await expect.poll(() => colourOf(a), { message: "a hovered link" }).toEqual(await c("var(--color-link-hover)"));
    }

    const strip = history.getByTestId("reply-strip").first();
    await expect(strip).toBeAttached();
    expect(contrast(await colourOf(strip), await c("var(--color-surface)")), "reply strip").toBeGreaterThanOrEqual(AA_TEXT);
  });
}
