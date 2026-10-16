import { test, expect } from "@playwright/test";
import { waitForApp } from "./example-room";

// Creating a room checks the room's first configuration and the creator's
// nickname against the new room's default limits before anything is signed
// (freenet/river#703): over a limit, the dialog says why and Create stays
// disabled; back within it, the room is created.

test.describe("Create-room limits", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("a nickname over the default limit is explained and blocks Create", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await page.getByTestId("create-room-button").click();
    const nickname = page.getByTestId("create-room-nickname-input");
    await expect(nickname).toBeVisible({ timeout: 10_000 });
    await page.getByTestId("create-room-name-input").fill("Limit Test Room");
    const submit = page.getByTestId("create-room-submit-button");
    const error = page.getByTestId("create-room-limit-error");

    await nickname.fill("A".repeat(51));
    await expect(error).toBeVisible();
    await expect(submit).toBeDisabled();

    await nickname.fill("A".repeat(50));
    await expect(error).toHaveCount(0);
    await expect(submit).toBeEnabled();
    await submit.click();
    await expect(page.getByTestId("create-room-modal")).toHaveCount(0);
    await expect(page.getByText("Limit Test Room").first()).toBeVisible();
  });

  test("a room name over the default limit is explained and blocks Create", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await page.getByTestId("create-room-button").click();
    const name = page.getByTestId("create-room-name-input");
    await expect(name).toBeVisible({ timeout: 10_000 });
    await page.getByTestId("create-room-nickname-input").fill("Alice");
    await name.fill("N".repeat(101));
    await expect(page.getByTestId("create-room-limit-error")).toBeVisible();
    await expect(page.getByTestId("create-room-submit-button")).toBeDisabled();
  });
});
