import { test, expect, Locator, Page } from "@playwright/test";
import { memberRows, openRoomWithComposer, resolveColor } from "./example-room";

// The dialogs share one type and button system (the phase 3 consistency
// pass): a 16px semibold title, no button filled with the accent (white on
// the dark accent is 2.15:1), and Cancel as the quiet ghost with no fill at
// rest. Checked on every dialog a viewer can reach from the example data,
// in both themes.

type Paint = { text: string; bg: string };

/// The title's type, and every button's text and resting fill, in the
/// dialog whose title is `title`: the nearest `fixed inset-0` layer above it.
async function dialogPaint(title: Locator): Promise<{ size: string; weight: number; buttons: Paint[] }> {
  return title.evaluate((h) => {
    let layer: Element | null = h;
    while (layer && !(layer.classList.contains("fixed") && layer.classList.contains("inset-0"))) layer = layer.parentElement;
    const scope = layer ?? h.parentElement!.parentElement!;
    const cs = getComputedStyle(h);
    return {
      size: cs.fontSize,
      weight: Number(cs.fontWeight),
      buttons: Array.from(scope.querySelectorAll("button")).map((b) => ({
        text: (b.textContent ?? "").trim(),
        bg: getComputedStyle(b).backgroundColor,
      })),
    };
  });
}

async function expectConsistent(page: Page, title: Locator, what: string) {
  await expect(title, `${what}: open`).toBeVisible();
  // Off every control, so no button is measured in its hover state.
  await page.mouse.move(0, 0);
  const p = await dialogPaint(title);
  expect(p.size, `${what}: title size`).toBe("16px");
  expect(p.weight, `${what}: title weight`).toBeGreaterThanOrEqual(600);
  const accents = [await resolveColor(page, "var(--color-accent)"), await resolveColor(page, "var(--color-accent-hover)")];
  expect(p.buttons.length, `premise: ${what} has buttons`).toBeGreaterThan(0);
  for (const b of p.buttons) {
    const bg = await resolveColor(page, b.bg);
    for (const accent of accents) expect(bg, `${what}: "${b.text}" is not filled with the accent`).not.toEqual(accent);
    if (/^Cancel/.test(b.text)) expect(bg[3], `${what}: "${b.text}" is a ghost`).toBe(0);
  }
}

for (const scheme of ["light", "dark"] as const) {
  test.describe(`Dialogs, ${scheme}`, { tag: "@chromium-only" }, () => {
    test.use({ colorScheme: scheme, viewport: { width: 1280, height: 800 } });

    test("create room, invite, invite code, import and export ID, member info", async ({ page }) => {
      await openRoomWithComposer(page);
      const close = async () => {
        await page.keyboard.press("Escape");
        await page.waitForTimeout(50);
      };
      const cases: [string, () => Promise<void>, string][] = [
        ["Create room", () => page.getByTestId("create-room-button").click(), "Create New Room"],
        ["Invite member", () => page.getByTestId("invite-member-button").click(), "Invite Member"],
        ["Enter invite code", () => page.getByTestId("join-with-code-button").click(), "Enter Invite Code"],
        ["Import ID", () => page.getByTestId("import-id-button").click(), "Import Identity"],
        ["Export ID", () => page.getByTestId("export-id-button").click(), "Export Identity"],
      ];
      for (const [what, open, heading] of cases) {
        await open();
        const title = page.getByRole("heading", { name: heading, exact: true });
        await expectConsistent(page, title, what);
        await close();
        if (await title.isVisible()) {
          // Not every dialog closes on Escape: use its Cancel or close button.
          const scope = page.locator(".fixed.inset-0").filter({ has: title }).last();
          await scope.getByRole("button", { name: /^(Cancel|Close)/ }).first().click();
        }
        await expect(title, `${what}: closed`).toBeHidden();
      }

      // Member info, for someone other than you.
      await memberRows(page).filter({ hasNotText: "you" }).first().click();
      await expectConsistent(page, page.getByRole("heading", { name: "Member Info", exact: true }), "Member info");
    });
  });
}
