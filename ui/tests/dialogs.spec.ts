import { test, expect, Locator, Page } from "@playwright/test";
import { resolveColor, selectRoom, waitForApp } from "./example-room";
import { DIALOG_CASES } from "./dialog-cases";

// The dialogs share one type and control system (the phase 3 consistency
// pass), checked on every dialog a viewer can reach, in both themes:
// - a 16px title of weight 600 or more; body text 15px, help text 13px;
// - text inputs in the composer's shape (rounded, on the surface colour);
// - no button filled with the accent (white on the dark accent is 2.15:1);
// - Cancel, Close and Decline as the quiet ghost (no fill at rest), any
//   other neutral-filled button raised with a hairline, and the primary, where
//   there is a neutral one, raised neutral: panel or surface fill and a border.

type Paint = { text: string; bg: string; border: number };
type Measured = {
  title: { size: string; weight: number };
  paragraphs: { size: string; text: string }[];
  inputs: { kind: string; radius: number; bg: string }[];
  buttons: Paint[];
};

/// What the dialog around `title` paints: the nearest `fixed inset-0` layer
/// above it, visible elements only.
async function measure(title: Locator): Promise<Measured> {
  return title.evaluate((h) => {
    let layer: Element | null = h;
    while (layer && !(layer.classList.contains("fixed") && layer.classList.contains("inset-0"))) layer = layer.parentElement;
    const scope = layer ?? h.parentElement!.parentElement!;
    const shown = (e: Element) => (e as HTMLElement).offsetParent !== null;
    return {
      title: { size: getComputedStyle(h).fontSize, weight: Number(getComputedStyle(h).fontWeight) },
      paragraphs: Array.from(scope.querySelectorAll("p"))
        .filter(shown)
        .map((p) => ({ size: getComputedStyle(p).fontSize, text: (p.textContent ?? "").trim().slice(0, 40) })),
      inputs: Array.from(scope.querySelectorAll("input, textarea, select"))
        .filter(shown)
        .filter((i) => !["checkbox", "radio", "hidden"].includes((i as HTMLInputElement).type))
        // The DM composer's draft sits inside its rounded box, like the room's.
        .filter((i) => !i.closest('[data-testid$="composer-box"]'))
        .map((i) => ({
          kind: (i as HTMLInputElement).type || i.tagName,
          radius: parseFloat(getComputedStyle(i).borderTopLeftRadius),
          bg: getComputedStyle(i).backgroundColor,
        })),
      buttons: Array.from(scope.querySelectorAll("button"))
        .filter(shown)
        .map((b) => ({
          text: (b.textContent ?? "").trim(),
          bg: getComputedStyle(b).backgroundColor,
          border: parseFloat(getComputedStyle(b).borderTopWidth),
        })),
    };
  });
}

for (const scheme of ["light", "dark"] as const) {
  test.describe(`Dialogs, ${scheme}`, { tag: "@chromium-only" }, () => {
    test.use({ colorScheme: scheme, viewport: { width: 1280, height: 800 } });

    for (const c of DIALOG_CASES) {
      test(c.name, async ({ page }) => {
        await page.goto(c.path ?? "/");
        await waitForApp(page);
        if (c.room) await selectRoom(page, c.room);
        await c.open(page);
        const title = c.title(page);
        await expect(title, "the dialog is open").toBeVisible({ timeout: 8_000 });
        // Off every control, so nothing is measured in its hover state.
        await page.mouse.move(0, 0);
        // Let colour transitions (open, un-hover) finish; skip endless ones.
        await page.evaluate(() =>
          Promise.all(
            document
              .getAnimations()
              .filter((a) => a.effect?.getComputedTiming().iterations !== Infinity)
              .map((a) => a.finished.catch(() => undefined)),
          ),
        );
        const m = await measure(title);
        const colour = (css: string) => resolveColor(page, css);
        const [accent, accentHover, link, panel, surface, surfaceHover] = await Promise.all(
          ["--color-accent", "--color-accent-hover", "--color-link", "--color-panel", "--color-surface", "--color-surface-hover"].map(
            (t) => colour(`var(${t})`),
          ),
        );
        const same = (a: number[], b: number[]) => a.every((v, i) => Math.abs(v - b[i]) <= 1);

        expect(m.title.size, "title size").toBe("16px");
        expect(m.title.weight, "title weight").toBeGreaterThanOrEqual(600);
        for (const p of m.paragraphs) expect(["15px", "13px"], `paragraph "${p.text}"`).toContain(p.size);
        for (const i of m.inputs) {
          expect(i.radius, `${i.kind} input radius`).toBeGreaterThanOrEqual(10);
          expect(same(await colour(i.bg), surface), `${i.kind} input on the surface colour`).toBe(true);
        }
        expect(m.buttons.length, "premise: the dialog has buttons").toBeGreaterThan(0);
        for (const b of m.buttons) {
          const bg = await colour(b.bg);
          for (const fill of [accent, accentHover, link]) expect(same(bg, fill), `"${b.text}" is not an accent fill`).toBe(false);
          if (/^(Cancel|Close|Decline)\b/.test(b.text)) expect(bg[3], `"${b.text}" is a ghost`).toBe(0);
          const neutral = bg[3] > 0 && [panel, surface, surfaceHover].some((n) => same(bg, n));
          if (neutral) expect(b.border, `"${b.text}" is raised with a hairline`).toBeGreaterThanOrEqual(1);
        }
        if (c.primary) {
          const primary = c.primary(page);
          await expect(primary).toBeVisible();
          const p = await primary.evaluate((el) => ({
            bg: getComputedStyle(el).backgroundColor,
            border: parseFloat(getComputedStyle(el).borderTopWidth),
          }));
          const bg = await colour(p.bg);
          expect([panel, surface].some((n) => same(bg, n)), "the primary is filled with the panel or surface colour").toBe(true);
          expect(p.border, "the primary has a border").toBeGreaterThanOrEqual(1);
        }
      });
    }
  });
}
