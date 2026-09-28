import { test, expect, Page, Locator } from "@playwright/test";
import { waitForApp, selectListedRoom, memberRows, resolveColor } from "./example-room";

// In "Team Chat Room" the owner's modal shows 👑 + 🎪 and the seeded impostor's shows ⚠ (ui/src/example_data.rs).

async function openModalFor(page: Page, badgeTestId: string) {
  await memberRows(page)
    .filter({ has: page.locator(`[data-testid="${badgeTestId}"]`) })
    .first()
    .click();
  await expect(page.getByTestId("member-info-modal")).toBeVisible({ timeout: 5_000 });
}

async function colourOf(chip: Locator) {
  await expect(chip).toBeVisible();
  return resolveColor(chip.page(), await chip.evaluate((el) => getComputedStyle(el).color));
}

test.describe("Member-info modal tag chips", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("status chips use --color-text; the warning chip keeps its own colour", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectListedRoom(page, "Team Chat Room");
    await expect(memberRows(page).first()).toBeVisible({ timeout: 5_000 });
    const text = await resolveColor(page, "var(--color-text)");

    await openModalFor(page, "member-list-owner");
    for (const tag of ["member-info-owner-tag", "member-info-invited-you-tag"]) {
      expect(await colourOf(page.getByTestId(tag)), tag).toEqual(text);
    }
    await page.getByTestId("member-info-close-button").click();
    await expect(page.getByTestId("member-info-modal")).toHaveCount(0, { timeout: 5_000 });

    // The member list paints before the impersonation sweep finishes, so
    // synchronise on the badge itself (see impersonation-warning.spec.ts).
    await expect(page.getByTestId("member-list-impersonation-warning").first()).toBeVisible({
      timeout: 15_000,
    });
    await openModalFor(page, "member-list-impersonation-warning");
    expect(await colourOf(page.getByTestId("member-info-impersonation-tag"))).not.toEqual(text);
  });
});
