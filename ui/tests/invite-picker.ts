import { expect, Locator, Page } from "@playwright/test";
import { selectRoom } from "./example-room";

// member_display_parts marks only the local user with ⭐.
export function isSelfRowText(text: string): boolean {
  return text.includes("⭐");
}

export function memberRows(page: Page): Locator {
  return page.getByTestId("member-list").locator('[data-testid^="member-item-"] button');
}

// Every example room lists the owner or self, the deputy and the impostor, so this never skips.
export async function openMemberInfoForFirstNonSelf(page: Page, room = "Team Chat Room") {
  await selectRoom(page, room);
  await expect(memberRows(page).first(), `${room}'s member list rendered`).toBeVisible({ timeout: 5_000 });
  for (const row of await memberRows(page).all()) {
    if (!isSelfRowText((await row.textContent()) ?? "")) {
      await row.click();
      return;
    }
  }
  throw new Error(`${room} lists no member other than the local user`);
}

export async function openShareInvitePicker(page: Page) {
  const share = page.getByTestId("member-info-share-invite-button");
  await expect(share, "the member-info modal offers 'Share an invite via DM'").toBeVisible({ timeout: 5_000 });
  await share.click();
  await expect(page.getByRole("heading", { name: /invite .+ to another room/i })).toBeVisible({ timeout: 5_000 });
}
