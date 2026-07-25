import { test, expect, Page } from "@playwright/test";

// The UX half of the nickname change: a nickname input tells the user why
// emoji aren't allowed instead of silently dropping the characters at render
// time.
//
// This is NOT the security boundary — riverctl writes member_info directly and
// never runs this code, which is why the render-time strip
// (`crate::util::display_name`) exists and is covered by Rust unit tests plus
// conversation-deputy-badge.spec.ts. This spec only pins that honest users get
// told.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
}

test.describe("Nickname inputs reject emoji", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the create-room nickname field explains and blocks", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await page.getByTestId("create-room-button").click();
    const nickname = page.getByTestId("create-room-nickname-input");
    await expect(nickname).toBeVisible({ timeout: 10_000 });

    await page.getByTestId("create-room-name-input").fill("Emoji Test Room");

    // A plain name is accepted with no complaint.
    await nickname.fill("Alice");
    await expect(
      page.getByTestId("create-room-nickname-emoji-error"),
    ).toHaveCount(0);
    await expect(nickname).toHaveAttribute("aria-invalid", "false");

    // Adding a shield surfaces the message and marks the field invalid.
    await nickname.fill("Alice 🛡");
    const error = page.getByTestId("create-room-nickname-emoji-error");
    await expect(error).toBeVisible();
    await expect(error).toHaveText("Nicknames can't contain emoji");
    await expect(nickname).toHaveAttribute("aria-invalid", "true");

    // Submit is disabled while invalid, matching the accept-invitation modal.
    const submit = page.getByTestId("create-room-submit-button");
    await expect(submit).toBeDisabled();

    // Clearing the emoji clears the error and re-enables submit, and the room
    // is really created. Without this positive control the assertion above
    // would also pass if the button were broken for an unrelated reason.
    await nickname.fill("Alice");
    await expect(
      page.getByTestId("create-room-nickname-emoji-error"),
    ).toHaveCount(0);
    await expect(submit).toBeEnabled();
    await submit.click();
    await expect(page.getByTestId("create-room-modal")).toHaveCount(0);
    await expect(page.getByText("Emoji Test Room").first()).toBeVisible();
  });

  test("a non-Latin nickname is NOT rejected", async ({ page }) => {
    // The rule must not mangle real names. A rule that flags 李小龍, محمد or
    // علی‌رضا (whose ZWNJ is orthography, not emoji machinery) is worse than
    // the problem it solves.
    await page.goto("/");
    await waitForApp(page);

    await page.getByTestId("create-room-button").click();
    const nickname = page.getByTestId("create-room-nickname-input");
    await expect(nickname).toBeVisible({ timeout: 10_000 });
    const error = page.getByTestId("create-room-nickname-emoji-error");

    for (const name of [
      "李小龍",
      "محمد عبد الله",
      "علی\u{200C}رضا", // Persian compound name, contains ZWNJ
      "සූර්\u{200D}ය", // Sinhala touching letters, contains ZWJ
      "山田\u{3000}太郎", // ideographic space separator
      "Иван Петров",
      "François Müller",
      "Nguyễn Thị Hương",
    ]) {
      // Drive the field INVALID first, and wait for the error to appear.
      // Asserting `toHaveCount(0)` straight after a `fill` would pass
      // instantly on a render that hasn't happened yet — i.e. the single most
      // important test here would be unable to fail for the right reason.
      await nickname.fill("x 🛡");
      await expect(error).toBeVisible();

      await nickname.fill(name);
      await expect(error, `rejected a legitimate name: ${name}`).toHaveCount(0);
    }
  });
});
