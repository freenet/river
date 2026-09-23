import { test, expect, Page, Locator } from "@playwright/test";

// The member-info modal's status chips (👑 Room Owner, 🎪 Invited You, ...)
// render their label in the body text colour, `--color-text`. The ⚠
// impersonation chip keeps its amber text, because there the colour is the
// warning. In "Team Chat Room" the owner's modal shows 👑 + 🎪 and the seeded
// impostor's shows ⚠ (ui/src/example_data.rs).

const MODAL = '[data-testid="member-info-modal"]';
const MEMBER_ROW = 'button[title^="Member ID"]';

async function openModalFor(page: Page, badgeTestId: string) {
  await page
    .locator(MEMBER_ROW)
    .filter({ has: page.locator(`[data-testid="${badgeTestId}"]`) })
    .first()
    .click();
  await expect(page.locator(MODAL)).toBeVisible({ timeout: 5_000 });
}

/**
 * The chip's text colour, and what `var(--color-text)` resolves to under the
 * same cascade. Both are serialized by the same engine, so string equality is
 * meaningful.
 */
async function chipColours(chip: Locator) {
  await expect(chip).toBeVisible();
  return chip.evaluate((el) => {
    const probe = document.createElement("span");
    probe.style.color = "var(--color-text)";
    el.parentElement!.appendChild(probe);
    try {
      return { chip: getComputedStyle(el).color, text: getComputedStyle(probe).color };
    } finally {
      probe.remove();
    }
  });
}

test.describe("Member-info modal tag chips", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("status chips use --color-text; the ⚠ chip keeps its warning colour", async ({
    page,
  }) => {
    await page.goto("/");
    await page.waitForSelector(".app-root", { timeout: 30_000 });
    await page.getByText("Team Chat Room").first().click();
    await page.locator(MEMBER_ROW).first().waitFor({ state: "visible", timeout: 15_000 });

    await openModalFor(page, "member-list-owner");
    for (const tag of ["member-info-owner-tag", "member-info-invited-you-tag"]) {
      const c = await chipColours(page.getByTestId(tag));
      expect(c.chip, tag).toBe(c.text);
    }
    await page.getByTestId("member-info-close-button").click();
    await expect(page.locator(MODAL)).toBeHidden({ timeout: 5_000 });

    // The member list paints before the impersonation sweep finishes, so
    // synchronise on the badge itself (see impersonation-warning.spec.ts).
    await page
      .locator('[data-testid="member-list-impersonation-warning"]')
      .first()
      .waitFor({ state: "visible", timeout: 15_000 });
    await openModalFor(page, "member-list-impersonation-warning");
    const warning = await chipColours(page.getByTestId("member-info-impersonation-tag"));
    expect(warning.chip).not.toBe(warning.text);
  });
});
