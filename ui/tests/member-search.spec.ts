import { test, expect } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// freenet/river#493. The member rows were already clickable. The missing
// piece is a name filter, opened from the Active Members heading so the
// rail stays quiet until someone asks for it.

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
    await page.getByTestId("member-search-toggle").click();
    const input = page.getByTestId("member-search-input");
    await expect(input).toBeVisible();

    const nickname = (
      await rows.nth(1).locator("span").first().innerText()
    ).trim();
    await input.fill(nickname);
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText(nickname);

    await input.fill("not-a-member-name-zzz");
    await expect(rows).toHaveCount(0);
    await expect(list.getByText("No matching members")).toBeVisible();

    await input.fill(nickname);
    await rows.first().click();
    await expect(page.getByTestId("member-info-modal")).toBeVisible();
  });
});
