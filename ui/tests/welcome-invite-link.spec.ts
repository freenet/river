import { test, expect, Page } from "@playwright/test";

// Regression test for issue #159: the Welcome screen (shown when no room
// is selected) must offer a concrete next step for new users — a link to
// the Freenet quickstart invite form so they can get into the "Freenet
// Official" room. The issue specifically called out that, especially on
// mobile, a brand-new user has no idea what to do after "Create a new
// room, or get invited to an existing one."

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

const INVITE_HREF = "https://freenet.org/quickstart#invite-form";

async function expectInviteLink(page: Page) {
  await page.goto("/");
  await waitForApp(page);

  // Confirm we're on the Welcome screen (no room selected on first load).
  await expect(page.getByText("Welcome to River")).toBeVisible({
    timeout: 5_000,
  });

  const link = page.locator(`a[href="${INVITE_HREF}"]`);
  await expect(link).toBeVisible({ timeout: 5_000 });
  await expect(link).toHaveText(
    /Click here to get an invitation to channel "Freenet Official"/
  );
  // External link must open in a new tab and be safe against tabnabbing.
  await expect(link).toHaveAttribute("target", "_blank");
  await expect(link).toHaveAttribute("rel", /noopener/);
}

test.describe("Welcome screen invite link (issue #159)", () => {
  test("desktop: invite link is shown on the Welcome screen", async ({
    page,
  }) => {
    await expectInviteLink(page);
  });

  test.describe("mobile viewport", () => {
    test.use({ viewport: { width: 375, height: 812 } });

    test("mobile: invite link is shown on the Welcome screen", async ({
      page,
    }) => {
      await expectInviteLink(page);
    });
  });
});
