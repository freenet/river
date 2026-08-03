import { test, expect, Page } from "@playwright/test";

// Regression test: quoted text (`> …` Markdown) inside a message bubble must
// stay legible, and must not invert against the bubble it sits in.
//
// The reported bug: in a SENT bubble — solid brand blue, every other word
// white — `.bg-accent .prose blockquote` painted the quote `rgba(0, 0, 0, 0.8)`.
// Quoted text rendered near-black beside white text at 3.52:1, under the 4.5:1
// WCAG AA floor. `.bg-accent` is the same blue in light and dark mode, so the
// bug was present in BOTH schemes even though it was reported against dark.
//
// The Rust pin (`conversation.rs::sent_bubble_blockquote_inherits_bubble_text_colour`)
// asserts the declaration text. This spec asserts what a reader actually sees:
// it injects a blockquote into a REAL rendered bubble, so the class list, the
// cascade and the compiled stylesheet are all the app's own, then composites
// the colours the browser computed and measures the contrast.

// ── WCAG 2.x relative luminance / contrast ──────────────────────────────────

type Rgb = [number, number, number];

function luminance([r, g, b]: Rgb): number {
  const channel = (c: number) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

function contrast(fg: Rgb, bg: Rgb): number {
  const a = luminance(fg);
  const b = luminance(bg);
  const [hi, lo] = a > b ? [a, b] : [b, a];
  return (hi + 0.05) / (lo + 0.05);
}

// ── Sampling the live DOM ───────────────────────────────────────────────────

interface Sample {
  background: Rgb;
  normal: Rgb;
  quote: Rgb;
  quoteBorder: Rgb;
}

/**
 * Inject a blockquote into a real message bubble, read back what the browser
 * computed, and remove it again.
 *
 * `kind` picks the bubble by the class the app itself renders: `bg-accent` is
 * the local user's own bubble (white text on brand blue), `bg-surface` is a
 * received one.
 */
async function sampleBubbleQuote(
  page: Page,
  kind: "bg-accent" | "bg-surface"
): Promise<Sample> {
  return page.evaluate((bubbleClass) => {
    // Resolve any CSS colour string — rgb(), oklab(), color-mix(), color() —
    // to straight-alpha sRGB by painting one pixel and reading it back.
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const ctx = canvas.getContext("2d", { willReadFrequently: true })!;
    const resolve = (css: string): { rgb: number[]; a: number } => {
      ctx.clearRect(0, 0, 1, 1);
      ctx.fillStyle = css;
      ctx.fillRect(0, 0, 1, 1);
      const d = ctx.getImageData(0, 0, 1, 1).data;
      const a = d[3] / 255;
      const straight = (c: number) => (a === 0 ? 0 : Math.min(255, c / a));
      return { rgb: [straight(d[0]), straight(d[1]), straight(d[2])], a };
    };

    const composite = (
      fg: { rgb: number[]; a: number },
      bg: number[]
    ): number[] => fg.rgb.map((c, i) => c * fg.a + bg[i] * (1 - fg.a));

    /**
     * The opaque colour actually behind `el`: walk ancestors collecting
     * backgrounds until an opaque one is found, then composite back down.
     * A bubble's own background can be translucent (the DM rail uses
     * `bg-accent/20`), so reading only the nearest one would measure against
     * a colour no pixel ever has.
     */
    const effectiveBackground = (el: Element): number[] => {
      const layers: { rgb: number[]; a: number }[] = [];
      let node: Element | null = el;
      while (node) {
        const c = resolve(getComputedStyle(node).backgroundColor);
        if (c.a > 0) {
          layers.push(c);
          if (c.a === 1) break;
        }
        node = node.parentElement;
      }
      // Nothing opaque anywhere up the chain: the canvas is white.
      let base = [255, 255, 255];
      for (let i = layers.length - 1; i >= 0; i--) base = composite(layers[i], base);
      return base;
    };

    const history = document.querySelector('[data-testid="conversation-history"]');
    if (!history) throw new Error("no conversation-history container rendered");

    // Find the message body whose NEAREST bubble ancestor is the requested
    // kind. A `.${bubbleClass} .prose` descendant selector would be wrong: it
    // matches whenever the class appears ANYWHERE above the body, so a sent
    // (`bg-accent`) bubble nested under some unrelated `bg-surface` wrapper
    // would answer to the received query and the test would silently measure
    // the wrong bubble.
    const prose = Array.from(history.querySelectorAll(".prose")).find((el) => {
      const bubble = el.closest(".bg-accent, .bg-surface");
      return bubble?.classList.contains(bubbleClass) ?? false;
    });
    if (!prose) {
      throw new Error(`no .${bubbleClass} bubble with a .prose body rendered`);
    }

    // Inject a quote and an ordinary paragraph as siblings inside the real
    // body container, so both are measured under the identical cascade.
    const probe = document.createElement("div");
    probe.innerHTML =
      "<p data-probe-normal>normal</p>" +
      "<blockquote data-probe-quote><p>quoted</p></blockquote>";
    prose.appendChild(probe);

    const normalEl = probe.querySelector("[data-probe-normal]")!;
    const quoteEl = probe.querySelector("[data-probe-quote]")!;

    const background = effectiveBackground(prose);
    const out = {
      background,
      normal: composite(resolve(getComputedStyle(normalEl).color), background),
      quote: composite(resolve(getComputedStyle(quoteEl).color), background),
      quoteBorder: composite(
        resolve(getComputedStyle(quoteEl).borderLeftColor),
        background
      ),
    };

    probe.remove();
    return out;
  }, kind) as Promise<Sample>;
}

async function openRoomWithMessages(page: Page) {
  await page.goto("/");
  await page.waitForSelector(".app-root", { timeout: 30_000 });

  // "Your Private Room" is the example-data room the local user owns, so it
  // carries both self-authored (bg-accent) and received (bg-surface) bubbles.
  const roomBtn = page.getByRole("button", { name: "Your Private Room" });
  await expect(roomBtn).toBeVisible({ timeout: 15_000 });
  await roomBtn.click();
  await expect(
    page.locator('[data-testid="conversation-history"] .prose')
  ).not.toHaveCount(0, { timeout: 15_000 });
}

// WCAG 2.1 AA: 4.5:1 for body text, 3:1 for the non-text graphics that carry
// meaning (here, the bar marking a quote as a quote).
const AA_TEXT = 4.5;
const AA_NON_TEXT = 3;

for (const colorScheme of ["dark", "light"] as const) {
  test.describe(`Blockquote contrast (${colorScheme} mode)`, () => {
    // A desktop viewport on every project: the mobile projects hide the
    // conversation pane behind view switching, which this test has no reason
    // to exercise — the stylesheet under test is viewport-independent.
    test.use({ colorScheme, viewport: { width: 1280, height: 900 } });

    test("quoted text in an own (accent) bubble matches the bubble's text", async ({
      page,
    }) => {
      await openRoomWithMessages(page);
      const s = await sampleBubbleQuote(page, "bg-accent");

      const quote = contrast(s.quote, s.background);
      const normal = contrast(s.normal, s.background);

      // The reported symptom, stated directly: the sent bubble is white text
      // on brand blue, and the quote used to be near-black — i.e. it sat on
      // the OPPOSITE side of the background's luminance from every other word
      // in the same bubble. This assertion fails on that regression even if
      // some future palette happened to keep the raw ratio above AA.
      const bgLum = luminance(s.background);
      expect(
        Math.sign(luminance(s.quote) - bgLum),
        `quoted text must not invert against its own bubble: normal text ` +
          `rgb(${s.normal.map(Math.round)}) vs quote ` +
          `rgb(${s.quote.map(Math.round)}) over rgb(${s.background.map(Math.round)})`
      ).toBe(Math.sign(luminance(s.normal) - bgLum));

      expect(quote, "quoted text must clear the WCAG AA floor").toBeGreaterThanOrEqual(
        AA_TEXT
      );

      // Inside the sent bubble the quote takes the bubble's own colour, so it
      // should read as no dimmer than the surrounding text. White on the
      // accent blue is only ~5.2:1 to begin with, which leaves no headroom to
      // dim a quote and stay above AA — hence `inherit` rather than a tint.
      expect(
        quote,
        "quoted text in an own bubble must be no dimmer than its normal text"
      ).toBeGreaterThanOrEqual(normal * 0.95);

      // The quote bar is the only marker left once the text colour matches;
      // it used to be `--color-accent-hover` on `--color-accent`, i.e. 1.29:1
      // and effectively invisible.
      expect(
        contrast(s.quoteBorder, s.background),
        "the quote bar must be visible against the bubble"
      ).toBeGreaterThanOrEqual(AA_NON_TEXT);
    });

    test("quoted text in a received (surface) bubble stays readable", async ({
      page,
    }) => {
      await openRoomWithMessages(page);
      const s = await sampleBubbleQuote(page, "bg-surface");

      const bgLum = luminance(s.background);
      expect(
        Math.sign(luminance(s.quote) - bgLum),
        `quoted text must not invert against its own bubble: normal text ` +
          `rgb(${s.normal.map(Math.round)}) vs quote ` +
          `rgb(${s.quote.map(Math.round)}) over rgb(${s.background.map(Math.round)})`
      ).toBe(Math.sign(luminance(s.normal) - bgLum));

      // A received quote IS deliberately muted relative to its normal text —
      // it just has to stay above AA while doing it, which the app-wide
      // `--color-text-muted` did not (4.47:1 in light mode).
      expect(
        contrast(s.quote, s.background),
        "quoted text must clear the WCAG AA floor"
      ).toBeGreaterThanOrEqual(AA_TEXT);
    });
  });
}
