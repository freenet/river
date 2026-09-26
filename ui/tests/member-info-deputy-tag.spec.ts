import { test, expect, Page } from "@playwright/test";
import { waitForApp, memberRows, selectRoom } from "./example-room";

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

async function openTeamChatMembers(page: Page) {
  await selectRoom(page, "Team Chat Room");
  await expect(memberRows(page).first()).toBeVisible({ timeout: 5_000 });
}

test.describe("Member-info modal deputy shield legend (#451)", { tag: "@chromium-only" }, () => {
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
    //
    // This count is ALSO the member-list regression gate for nickname emoji
    // stripping: example_data.rs deliberately gives the OWNER the stored
    // nickname "… (Owner) 🛡👑", so if `crate::util::display_name` ever stops
    // stripping it, the owner's row matches too and this becomes 2.
    const deputyRow = memberRows(page).filter({ hasText: "🛡" });
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
    await memberRows(page).filter({ hasNotText: "🛡" }).first().click();

    await expect(page.getByTestId("member-info-modal")).toBeVisible({
      timeout: 5_000,
    });
    // The deputy chip must be absent for a member who is not a deputy.
    await expect(page.locator(DEPUTY_TAG)).toHaveCount(0);
  });
});
