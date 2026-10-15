import { test, expect } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// freenet/river#493. The member rows were already clickable. The missing
// piece is a name filter, opened from the Active Members heading so the
// rail stays quiet until someone asks for it.

test.describe("Member search on a phone", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("the lens sits beside Active Members and the field stays on screen", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await page.getByTestId("header-members-button").click();

    const toggle = page.getByTestId("member-search-toggle");
    await expect(toggle).toBeVisible();
    await expect(toggle).toContainText("Active Members");
    const lens = toggle.locator("svg").nth(1);
    await expect(lens).toBeVisible();
    const toggleBox = await toggle.boundingBox();
    const lensBox = await lens.boundingBox();
    expect(toggleBox).not.toBeNull();
    expect(lensBox).not.toBeNull();
    expect(lensBox!.x).toBeGreaterThan(toggleBox!.x);
    expect(lensBox!.x + lensBox!.width).toBeLessThanOrEqual(390);

    await toggle.click();
    const input = page.getByTestId("member-search-input");
    await expect(input).toBeVisible();
    const inputBox = await input.boundingBox();
    expect(inputBox).not.toBeNull();
    expect(inputBox!.x).toBeGreaterThanOrEqual(0);
    expect(inputBox!.x + inputBox!.width).toBeLessThanOrEqual(390);
  });
});

test.describe("Member search", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("clicking Active Members filters the list and a row still opens info", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const list = page.getByTestId("member-list");
    const rows = list.locator('[data-testid^="member-item-"]');
    await expect(rows.first()).toBeVisible();
    const before = await rows.count();
    expect(before).toBeGreaterThan(1);

    await expect(page.getByTestId("member-search-input")).toHaveCount(0);
    const toggle = page.getByTestId("member-search-toggle");
    // Still a heading, with the toggle inside it.
    await expect(page.getByRole("heading", { name: "Active Members" })).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-controls", "member-search-input");
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect((await toggle.boundingBox())!.height).toBeGreaterThanOrEqual(24);
    await toggle.click();
    const input = page.getByTestId("member-search-input");
    await expect(input).toBeVisible();
    await expect(input).toBeFocused();
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    await expect(page.getByRole("searchbox", { name: "Search members by name" })).toBeVisible();

    const nickname = (
      await rows.nth(1).locator("span").first().innerText()
    ).trim();
    await input.fill(nickname);
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText(nickname);

    // Case does not matter, either way round.
    await input.fill(nickname.toUpperCase());
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText(nickname);
    await input.fill(nickname.toLowerCase());
    await expect(rows).toHaveCount(1);

    // The live region is in the page before there is anything to announce,
    // so a screen reader is already watching it when the text arrives.
    const status = page.getByTestId("member-search-status");
    await expect(status).toHaveAttribute("role", "status");
    await expect(status).toHaveText("");
    await input.fill("not-a-member-name-zzz");
    await expect(rows).toHaveCount(0);
    await expect(list.getByText("No matching members")).toBeVisible();
    await expect(status).toHaveText("No matching members");

    await input.fill(nickname);
    await rows.first().click();
    await expect(page.getByTestId("member-info-modal")).toBeVisible();
    await page.getByTestId("member-info-close-button").click();

    await input.press("Escape");
    await expect(page.getByTestId("member-search-input")).toHaveCount(0);
    await expect(toggle).toBeFocused();
    await expect(rows).toHaveCount(before);
  });

  test("switching rooms closes the search and clears the query", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const rows = page.getByTestId("member-list").locator('[data-testid^="member-item-"]');
    await expect(rows.first()).toBeVisible();
    const toggle = page.getByTestId("member-search-toggle");
    await toggle.click();
    await page.getByTestId("member-search-input").fill("not-a-member-name-zzz");
    await expect(rows).toHaveCount(0);

    await selectRoom(page, "Public Discussion Room");
    await expect(page.getByTestId("member-search-input")).toHaveCount(0);
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await expect(rows.first()).toBeVisible();
    await expect(page.getByText("No matching members")).toHaveCount(0);

    // Coming back does not bring the old query with it.
    await selectRoom(page, "Team Chat Room");
    await expect(rows.first()).toBeVisible();
    await expect(page.getByTestId("member-search-input")).toHaveCount(0);
  });
});
