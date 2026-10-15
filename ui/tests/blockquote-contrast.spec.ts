import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";
import { openMemberInfoForFirstNonSelf } from "./invite-picker";

// Regression test: quoted text (`> …` Markdown) in a message must stay
// legible, and must not invert against the surface it sits on.
//
// The reported bug was in the old solid-blue SENT bubble, where the quote
// painted near-black beside white text (3.52:1). Messages now share one row
// layout on the conversation background, so own and received rows are checked
// alike, in both colour schemes, together with the tinted outgoing DM bubble.
//
// The Rust pins in `conversation/markdown.rs` assert the declaration TEXT. This spec
// asserts what a reader actually sees: it puts a quote in a REAL rendered
// message, so the class list, the cascade and the compiled stylesheet are all
// the app's own, then composites the colours the browser computed.

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

// WCAG 2.1 AA: 4.5:1 for body text.
const AA_TEXT = 4.5;

// ── Sampling the live DOM ───────────────────────────────────────────────────

interface Sample {
  background: Rgb;
  normal: Rgb;
  quote: Rgb;
  quoteBorder: Rgb;
}

/** Which real surface to measure. */
type Surface = "own" | "received" | "dm";

/**
 * Put a blockquote in a real message on `surface`, read back what the browser
 * computed for it, and clean up after.
 *
 * The room-message surfaces are probed by injecting into an existing row's
 * body; the DM surface has no example-data messages at all, so its bubble is
 * created by actually sending a DM (see `openDmThread`).
 */
async function sampleQuote(page: Page, surface: Surface): Promise<Sample> {
  const raw = await page.evaluate((which: Surface) => {
    // A colour the app never uses, so "fillStyle did not change" is
    // unambiguous. `fillStyle` normalises to lowercase hex.
    const SENTINEL = "#ff00ff";

    // Resolve any CSS colour string — rgb(), oklab(), color-mix(), color() —
    // to sRGB by painting one pixel and reading it back.
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const ctx = canvas.getContext("2d", { willReadFrequently: true })!;
    const resolve = (css: string): { rgb: number[]; a: number } => {
      ctx.clearRect(0, 0, 1, 1);
      // Assigning an unparseable value to `fillStyle` is a no-op — it keeps
      // the PREVIOUS value, in chromium, firefox and webkit alike. Without a
      // sentinel, a colour syntax some engine doesn't support would silently
      // measure whatever was sampled before it, which reads as a real number.
      ctx.fillStyle = SENTINEL;
      ctx.fillStyle = css;
      if (ctx.fillStyle === SENTINEL && css.toLowerCase() !== SENTINEL) {
        throw new Error(`browser could not parse the colour: ${css}`);
      }
      ctx.fillRect(0, 0, 1, 1);
      const d = ctx.getImageData(0, 0, 1, 1).data;
      // `getImageData` returns STRAIGHT (non-premultiplied) alpha — verified
      // in all three engines: `rgba(255,0,0,0.2)` reads back as [255,0,0,51],
      // not [51,0,0,51]. Do NOT divide by alpha here. An earlier revision did,
      // which inflated and clamped every translucent colour: the
      // `bg-accent/20` DM bubble resolved to a nonsense rgb(204,255,255)
      // instead of rgb(204,224,250), and the fabricated contrast number that
      // produced was quoted as evidence before anyone measured the real modal.
      return { rgb: [d[0], d[1], d[2]], a: d[3] / 255 };
    };

    const over = (fg: { rgb: number[]; a: number }, bg: number[]): number[] =>
      fg.rgb.map((c, i) => c * fg.a + bg[i] * (1 - fg.a));

    /**
     * The opaque colour actually behind `el`: walk ancestors collecting
     * backgrounds until an opaque one is found, then composite back down.
     * A bubble's own background can be translucent — the outgoing DM bubble
     * is `bg-accent/20` — so reading only the nearest one would measure
     * against a colour no pixel ever has.
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
      for (let i = layers.length - 1; i >= 0; i--) base = over(layers[i], base);
      return base;
    };

    let bubble: Element;
    let quoteEl: Element;
    let normalEl: Element;
    let cleanup = () => {};

    if (which === "dm") {
      // The outgoing DM bubble IS the `.prose` container (dm_thread_modal.rs),
      // and it already holds a real `<blockquote>` — `openDmThread` sent one.
      const container = document.querySelector("#dm-scroll-container");
      if (!container) throw new Error("DM thread modal is not open");
      const bq = container.querySelector("blockquote");
      if (!bq) throw new Error("no blockquote in the DM thread");
      bubble = bq.closest("[class*='bg-accent']") ?? bq.parentElement!;
      quoteEl = bq;
      const p = Array.from(bubble.querySelectorAll("p")).find(
        (el) => !el.closest("blockquote")
      );
      if (!p) throw new Error("DM bubble has no non-quoted paragraph");
      normalEl = p;
    } else {
      const history = document.querySelector(
        '[data-testid="conversation-history"]'
      );
      if (!history) throw new Error("no conversation-history container");

      // A message body in a row by the requested author.
      const wanted = which === "own" ? "true" : "false";
      const prose = Array.from(
        history.querySelectorAll('[data-testid="message-body"] .prose')
      ).find(
        (el) =>
          el.closest("[data-message-self]")?.getAttribute("data-message-self") === wanted
      );
      if (!prose) throw new Error(`no ${which} message with a .prose body`);

      // Inject a quote and an ordinary paragraph as siblings inside the real
      // body container, so both are measured under the identical cascade.
      const probe = document.createElement("div");
      probe.innerHTML =
        "<p data-probe-normal>normal</p>" +
        "<blockquote data-probe-quote><p>quoted</p></blockquote>";
      prose.appendChild(probe);
      cleanup = () => probe.remove();

      bubble = prose;
      normalEl = probe.querySelector("[data-probe-normal]")!;
      quoteEl = probe.querySelector("[data-probe-quote]")!;
    }

    try {
      const background = effectiveBackground(bubble);
      return {
        background,
        normal: over(resolve(getComputedStyle(normalEl).color), background),
        quote: over(resolve(getComputedStyle(quoteEl).color), background),
        quoteBorder: over(
          resolve(getComputedStyle(quoteEl).borderLeftColor),
          background
        ),
      };
    } finally {
      cleanup();
    }
  }, surface);

  return raw as Sample;
}

// ── Fixtures ────────────────────────────────────────────────────────────────

async function openRoomWithMessages(page: Page) {
  await page.goto("/");
  await waitForApp(page);

  // "Your Private Room" is the example-data room the local user owns, so it
  // carries both own and received messages.
  await selectRoom(page, "Your Private Room");
  await expect(
    page.locator('[data-testid="conversation-history"] .prose')
  ).not.toHaveCount(0, { timeout: 15_000 });
}

/**
 * Open a DM thread and send one quoted message into it.
 *
 * Example data ships zero DMs, so the outgoing bubble has to be created. The
 * member-info → "Send direct message" route mirrors `dm-thread-modal.spec.ts`.
 */
async function openDmThread(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await openMemberInfoForFirstNonSelf(page);
  await page.getByTestId("member-info-dm-button").click();
  await page.waitForSelector("#dm-scroll-container", { timeout: 15_000 });

  const composer = page.getByPlaceholder("Type a direct message...");
  await composer.fill("They wrote:\n\n> this is quoted text\n\nMy reply.");
  await page.keyboard.press("Enter");
  await page
    .locator("#dm-scroll-container blockquote")
    .first()
    .waitFor({ timeout: 15_000 });
}

// ── Assertions shared by every surface ──────────────────────────────────────

function expectNotInverted(s: Sample) {
  // The reported symptom, stated directly: the quote used to sit on the
  // OPPOSITE side of the background's luminance from every other word in the
  // same message. This fails on that regression even if some future palette
  // happened to keep the raw ratio above AA.
  const bg = luminance(s.background);
  expect(
    Math.sign(luminance(s.quote) - bg),
    `quoted text must not invert against its own bubble: normal text ` +
      `rgb(${s.normal.map(Math.round)}) vs quote ` +
      `rgb(${s.quote.map(Math.round)}) over rgb(${s.background.map(Math.round)})`
  ).toBe(Math.sign(luminance(s.normal) - bg));
}

for (const colorScheme of ["dark", "light"] as const) {
  test.describe(`Blockquote contrast (${colorScheme} mode)`, () => {
    // A desktop viewport on every project: the mobile projects hide the
    // conversation pane behind view switching, which this test has no reason
    // to exercise — the stylesheet under test is viewport-independent.
    test.use({ colorScheme, viewport: { width: 1280, height: 900 } });

    // A quote is deliberately dimmer than its normal text — it just has to
    // stay above AA while doing it, which the app-wide `--color-text-muted`
    // did not in light mode (3.96:1 on the outgoing DM bubble). Both bounds
    // are asserted: without the upper one, deleting `--color-text-quote`'s
    // light value makes `var()` invalid, the quote inherits full body colour,
    // and every floor-only assertion still passes.
    for (const surface of ["own", "received", "dm"] as const) {
      const label = { own: "an own message", received: "a received message", dm: "an outgoing DM bubble" }[surface];
      test(`quoted text in ${label} is readable and still muted`, async ({
        page,
      }) => {
        if (surface === "dm") {
          await openDmThread(page);
        } else {
          await openRoomWithMessages(page);
        }
        const s = await sampleQuote(page, surface);

        const quote = contrast(s.quote, s.background);
        const normal = contrast(s.normal, s.background);

        expectNotInverted(s);
        expect(
          quote,
          "quoted text must clear the WCAG AA floor"
        ).toBeGreaterThanOrEqual(AA_TEXT);
        expect(
          quote,
          "a quote must stay visibly secondary to its normal text, not " +
            "collapse into the body colour"
        ).toBeLessThan(normal * 0.8);
      });
    }
  });
}
