import { test, expect, Locator } from "@playwright/test";
import { colourOf, contrast, effectiveBackground, over, resolveColor, selectRoom, waitForApp } from "./example-room";

// Text in a message row sits on the conversation surface, the row's hover
// band, your own rows' tint (at rest and hovered), or a mention chip's fill
// over any of those. Links, mentions, the own name, quotes and times must
// clear WCAG AA (4.5:1) in both themes, measured from what the browser
// renders; a quote must also stay visibly dimmer than body text.
// blockquote-contrast.spec.ts covers the quote on the outgoing DM bubble.

const AA_TEXT = 4.5;

for (const scheme of ["light", "dark"] as const) {
  test(`${scheme}: links, mentions, quotes, times and reply strips clear 4.5:1`, { tag: "@chromium-only" }, async ({ page }) => {
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

    // The surfaces, as painted: a received row's background is the
    // conversation surface, not a token the test assumes.
    const bg = await effectiveBackground(await rendered('[data-message-self="false"]'));
    const ownRest = await effectiveBackground(await rendered('[data-message-self="true"]'));
    expect(ownRest, "premise: own rows are tinted").not.toEqual(bg);
    const band = over(await c("var(--color-row-hover)"), bg);
    const ownHover = over(await c("var(--color-row-self-hover)"), bg);
    const rows: Record<string, number[]> = { bg, band, "own row": ownRest, "own row hovered": ownHover };
    const surfaces: Record<string, number[]> = { ...rows };
    for (const cls of ["river-mention", "river-mention river-mention-self"]) {
      const fill = await probe(`<span class="${cls}">x</span>`, async (el) =>
        c(await el.evaluate((e) => getComputedStyle(e).backgroundColor)),
      );
      for (const [name, s] of Object.entries(rows)) surfaces[`${cls} on ${name}`] = over(fill, s);
    }

    // Link hover, read off a hovered link once its colour transition ends.
    const link = await rendered(".prose a");
    await link.scrollIntoViewIfNeeded();
    const linkRest = await colourOf(link);
    await link.hover();
    const linkHover = await link.evaluate(async (el) => {
      await Promise.all(el.getAnimations().map((a) => a.finished));
      return getComputedStyle(el).color;
    });
    expect(await c(linkHover), "premise: the hovered link changed colour").not.toEqual(linkRest);
    await page.mouse.move(0, 0);

    const linkLike: Record<string, number[]> = {
      link: linkRest,
      "hovered link": await c(linkHover),
      mention: await colourOf(await rendered(".river-mention")),
      "own name": await colourOf(await rendered('[data-message-self="true"] [data-testid="message-author-name"]')),
    };
    for (const [what, fg] of Object.entries(linkLike)) {
      for (const [name, s] of Object.entries(surfaces)) {
        expect(contrast(fg, s), `${what} on ${name}`).toBeGreaterThanOrEqual(AA_TEXT);
      }
    }

    const quote = await probe("<blockquote><p>quoted</p></blockquote>", colourOf);
    const time = await colourOf(await rendered('[data-testid="message-group-header"] span[title]:last-child'));
    const normal = await colourOf(await rendered('[data-testid="message-body"] .prose p'));
    for (const [name, s] of Object.entries(rows)) {
      expect(contrast(quote, s), `quote on ${name}`).toBeGreaterThanOrEqual(AA_TEXT);
      expect(contrast(time, s), `time on ${name}`).toBeGreaterThanOrEqual(AA_TEXT);
    }
    expect(contrast(quote, bg), "a quote stays dimmer than body text").toBeLessThan(contrast(normal, bg) * 0.8);

    // Every reply strip, received and own, against what is painted behind it.
    const strips = history.locator('[data-testid="reply-strip"]');
    expect(await strips.count(), "premise: reply strips are rendered").toBeGreaterThan(0);
    for (const strip of await strips.all()) {
      const behind = await effectiveBackground(strip);
      expect(contrast(over(await colourOf(strip), behind), behind), "reply strip").toBeGreaterThanOrEqual(AA_TEXT);
    }
  });
}
