import { test, expect, Locator, Page } from "@playwright/test";
import { colourOf, contrast, effectiveBackground, hamburgerBadge, memberRows, openRoomWithComposer, selectRoom, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// The shell's text against what is painted behind it, in both themes: the
// composer, the rooms rail (including the link-coloured text on accent-soft
// pills) and the member panel; on a phone, the hamburger's unread count.
// AA: 4.5:1 for text, 3:1 for an icon-only control (WCAG 1.4.11).

async function expectContrast(page: Page, el: Locator, min: number, what: string) {
  await expect(el, `${what} is shown`).toBeVisible();
  const ratio = contrast(await colourOf(el), await effectiveBackground(el));
  expect(ratio, `${what}: ${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(min);
}

for (const scheme of ["light", "dark"] as const) {
  test.describe(`Shell contrast, ${scheme}`, { tag: "@chromium-only" }, () => {
    test.use({ colorScheme: scheme });

    test.describe("desktop", () => {
      test.use({ viewport: { width: 1280, height: 800 } });

      test("composer, rooms rail and member panel", async ({ page }) => {
        await openRoomWithComposer(page);
        // Composer: the draft text, and the Send icon (enabled by a draft).
        const input = page.getByTestId("message-input");
        await input.fill("a draft");
        await expectContrast(page, input, 4.5, "the composer draft");
        await expectContrast(page, page.getByTestId("send-message-button"), 3, "the Send icon");
        await expectContrast(page, page.getByTestId("composer-emoji-button"), 3, "the emoji button");

        // Rooms rail: headings, the open room, a closed room, the unread
        // pill (link text on accent-soft), the actions and the footer.
        const rail = page.getByTestId("rooms-rail");
        await expectContrast(page, rail.getByRole("heading", { name: "Rooms", exact: true }), 4.5, "the Rooms heading");
        const rooms = page.getByTestId("room-list");
        await expectContrast(page, rooms.getByRole("button", { name: "Your Private Room" }), 4.5, "the open room");
        await expectContrast(page, rooms.getByRole("button", { name: "Team Chat Room" }).locator("span").first(), 4.5, "a closed room");
        await expectContrast(page, page.getByTestId("room-unread-badge").first(), 4.5, "an unread pill");
        await expectContrast(page, rail.getByTestId("join-with-code-button"), 4.5, "Enter invite code");
        await expectContrast(page, rail.getByTestId("import-id-button"), 4.5, "Import ID");

        // Member panel: names, the "you" marker, the header and Invite.
        await expectContrast(page, memberRows(page).first().getByTestId("member-name"), 4.5, "a member name");
        await expectContrast(page, page.getByTestId("member-list-self"), 4.5, "the 'you' marker");
        await expectContrast(page, page.getByTestId("invite-member-button"), 4.5, "Invite someone");
        await expectContrast(page, page.getByTestId("members-panel-toggle"), 3, "the members toggle");
      });

      test("a DM row's unread pill in the rail", async ({ page }) => {
        await page.goto("/");
        await waitForApp(page);
        await selectRoom(page, "Team Chat Room");
        await callRiverTest(page, "appendDmsForPeer", 0, 2);
        await expectContrast(page, page.getByTestId("dm-rail-unread-badge").first(), 4.5, "a DM unread pill");
      });
    });

    test.describe("phone", () => {
      test.use({ viewport: { width: 390, height: 844 } });

      test("the hamburger's unread count", async ({ page }) => {
        await openRoomWithComposer(page);
        // Example data seeds unread messages in the other rooms.
        await expectContrast(page, hamburgerBadge(page), 4.5, "the hamburger badge");
      });
    });
  });
}
