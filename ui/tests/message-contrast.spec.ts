import { test, expect, Locator } from "@playwright/test";
import { colourOf, contrast, over, resolveColor, selectRoom, waitForApp } from "./example-room";

// Text in a message row sits on the conversation background, the row's hover
// band, or a mention chip's fill over either. Links, mentions, the own name
// and quotes must clear WCAG AA (4.5:1) in both themes, measured from what the
// browser renders; a quote must also stay visibly dimmer than body text.
// blockquote-contrast.spec.ts covers the quote on the outgoing DM bubble.

const AA_TEXT = 4.5;

for (const scheme of ["light", "dark"] as const) {
  test(`${scheme}: links, mentions, quotes and reply strips clear 4.5:1`, { tag: "@chromium-only" }, async ({ page }) => {
    await page.emulateMedia({ colorScheme: scheme });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    const c = (css: string) => resolveColor(page, css);
    const history = page.locator("#chat-scroll-container");
    const rendered = async (sel: string) => {
      const el = history.locator(sel).first();
      await expect(el, `premise: ${sel} is rendered`).toBeAttached();
      return el;
    };
    // A probe carrying real classes inside the history, so the cascade is the app's.
    const probe = async (html: string, read: (el: Locator) => Promise<number[]>) => {
      await page.evaluate((html) => {
        const host = document.querySelector('[data-message-self="false"] [data-testid="message-body"] .prose')!;
        host.insertAdjacentHTML("beforeend", `<div data-probe>${html}</div>`);
      }, html);
      try {
        return await read(history.locator("[data-probe] > *").first());
      } finally {
        await page.evaluate(() => document.querySelector("[data-probe]")?.remove());
      }
    };

    const bg = await c("var(--color-bg)");
    const band = over(await c("var(--color-row-hover)"), bg);
    const surfaces: Record<string, number[]> = { bg, band };
    for (const cls of ["river-mention", "river-mention river-mention-self"]) {
      const fill = await probe(`<span class="${cls}">x</span>`, async (el) =>
        c(await el.evaluate((e) => getComputedStyle(e).backgroundColor)),
      );
      surfaces[`${cls} on bg`] = over(fill, bg);
      surfaces[`${cls} on band`] = over(fill, band);
    }

    // An undefined hover token would resolve to the inherited text colour and pass.
    const hoverDeclared = await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue("--color-link-hover").trim());
    expect(hoverDeclared, "premise: --color-link-hover is declared").not.toBe("");
    const linkLike: Record<string, number[]> = {
      link: await colourOf(await rendered(".prose a")),
      mention: await colourOf(await rendered(".river-mention")),
      "own name": await colourOf(await rendered('[data-message-self="true"] [data-testid="message-author-name"]')),
      "link hover": await c("var(--color-link-hover)"),
    };
    for (const [what, fg] of Object.entries(linkLike)) {
      for (const [name, s] of Object.entries(surfaces)) {
        expect(contrast(fg, s), `${what} on ${name}`).toBeGreaterThanOrEqual(AA_TEXT);
      }
    }

    const quote = await probe("<blockquote><p>quoted</p></blockquote>", colourOf);
    const normal = await colourOf(await rendered('[data-testid="message-body"] .prose p'));
    for (const [name, s] of Object.entries({ bg, band })) {
      expect(contrast(quote, s), `quote on ${name}`).toBeGreaterThanOrEqual(AA_TEXT);
    }
    expect(contrast(quote, bg), "a quote stays dimmer than body text").toBeLessThan(contrast(normal, bg) * 0.8);

    const strip = await rendered('[data-testid="reply-strip"]');
    expect(contrast(await colourOf(strip), await c("var(--color-surface)")), "reply strip").toBeGreaterThanOrEqual(AA_TEXT);
  });
}
