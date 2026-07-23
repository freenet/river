import { test, expect, Page } from "@playwright/test";

// Render coverage for freenet/river#451: the member-info modal legend must
// show the 🛡 deputy chip for a member who carries the shield in the member
// list, under the SAME viewer-relevant condition as the list row.
//
// Example data (ui/src/example_data.rs) has the "Team Chat Room" owner
// deputize the "(Member)" member, so that member is a global moderator and
// shows the shield in every view. The owner and the local "(You)" member are
// not deputies, so their modals must NOT show the chip.
//
// Rust unit tests pin the decision logic (`relevant_deputizer_names`) and that
// the modal is wired to the shared helper; this spec pins the end-to-end
// render — the exact regression that was reported (icon in the list, missing
// from the info page) — which a source-grep pin cannot catch.

const DEPUTY_TAG = '[data-testid="member-info-deputy-tag"]';

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
}

async function openTeamChatMembers(page: Page) {
  await page.getByText("Team Chat Room").first().click();
  await page
    .locator('button[title^="Member ID"]')
    .first()
    .waitFor({ state: "visible", timeout: 5_000 })
    .catch(() => undefined);
}

test.describe("Member-info modal deputy shield legend (#451)", () => {
  // Fixed desktop viewport so the member list is always in-panel (mirrors
  // dm-thread-modal.spec.ts).
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the deputy member's info modal shows the 🛡 Deputy chip", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openTeamChatMembers(page);

    // The deputy is the one member whose row carries the shield glyph. Exactly
    // one member (the owner-appointed "(Member)") is a deputy in example data.
    const deputyRow = page
      .locator('button[title^="Member ID"]')
      .filter({ hasText: "🛡" });
    await expect(deputyRow).toHaveCount(1);

    await deputyRow.first().click();

    await expect(page.getByTestId("member-info-modal")).toBeVisible({
      timeout: 5_000,
    });
    const tag = page.locator(DEPUTY_TAG);
    await expect(tag).toBeVisible();
    await expect(tag).toContainText("Deputy");
    // Tooltip names the appointer (the owner).
    await expect(tag).toHaveAttribute("title", /appointed by/i);
  });

  test("a non-deputy member's info modal does NOT show the chip", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openTeamChatMembers(page);

    // Pick a member row WITHOUT the shield (the owner or the local user).
    const rows = page.locator('button[title^="Member ID"]');
    const count = await rows.count();
    let openedNonDeputy = false;
    for (let i = 0; i < count; i++) {
      const text = (await rows.nth(i).textContent()) || "";
      if (!text.includes("🛡")) {
        await rows.nth(i).click();
        openedNonDeputy = true;
        break;
      }
    }
    expect(openedNonDeputy).toBe(true);

    await expect(page.getByTestId("member-info-modal")).toBeVisible({
      timeout: 5_000,
    });
    // The deputy chip must be absent for a member who is not a deputy.
    await expect(page.locator(DEPUTY_TAG)).toHaveCount(0);
  });
});
