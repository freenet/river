import { expect, Locator, Page } from "@playwright/test";
import { memberRows, selectRoom } from "./example-room";

// member_display_parts marks only the local user with ⭐.
export function nonSelfMemberRows(page: Page): Locator {
  return memberRows(page).filter({ hasNotText: "⭐" });
}

export function pickerHeading(page: Page): Locator {
  return page.getByRole("heading", { name: /invite .+ to another room/i });
}

// Every example room lists a member other than the local user (the deputy, the impostor).
export async function openMemberInfoForFirstNonSelf(page: Page) {
  await selectRoom(page, "Team Chat Room");
  const row = nonSelfMemberRows(page).first();
  await expect(row, "Team Chat Room lists a member other than the local user").toBeVisible({ timeout: 5_000 });
  await row.click();
}

export async function openShareInvitePicker(page: Page) {
  const share = page.getByTestId("member-info-share-invite-button");
  await expect(share, "the member-info modal offers 'Share an invite via DM'").toBeVisible({ timeout: 5_000 });
  await share.click();
  await expect(pickerHeading(page)).toBeVisible({ timeout: 5_000 });
}
