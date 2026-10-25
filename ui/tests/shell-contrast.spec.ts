import { test, expect, Locator, Page } from "@playwright/test";
import { colourOf, contrast, effectiveBackground, hamburgerBadge, memberRows, openRoomWithComposer, over, selectRoom, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";
import { DM_PEER, dm, openThread } from "./dm-thread";

// The shell's text against what is painted behind it, in both themes: the
// composer, the rooms rail (including the link-coloured text on accent-soft
// pills) and the member panel; on a phone, the hamburger's unread count.
// AA: 4.5:1 for text, 3:1 for an icon-only control (WCAG 1.4.11).

/// The text as painted: its colour's alpha times the opacity of it and its
/// ancestors, composited over what is behind it.
async function expectContrast(page: Page, el: Locator, min: number, what: string) {
  await expect(el, `${what} is shown`).toBeVisible();
  const opacity = await el.evaluate(async (e) => {
    await Promise.all(e.getAnimations({ subtree: false }).map((a) => a.finished));
    let o = 1;
    for (let n: Element | null = e; n; n = n.parentElement) o *= parseFloat(getComputedStyle(n).opacity);
    return o;
  });
  const fg = await colourOf(el);
  const bg = await effectiveBackground(el);
  const ratio = contrast(over([fg[0], fg[1], fg[2], (fg[3] ?? 1) * opacity], bg), bg);
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

      test("a continuation row's gutter time, shown on hover", async ({ page }) => {
        test.skip(await page.evaluate(() => window.matchMedia("(hover: none), (any-pointer: coarse)").matches), "no gutter time on touch");
        await openRoomWithComposer(page);
        const input = page.getByTestId("message-input");
        for (const text of ["first of a pair", "second of a pair"]) {
          await input.fill(text);
          await input.press("Enter");
          await expect(page.getByText(text)).toHaveCount(1);
        }
        const row = page.getByTestId("message-row").filter({ hasText: "second of a pair" });
        const time = row.getByTestId("message-gutter-time");
        await row.getByTestId("message-body").hover();
        await expect.poll(() => time.evaluate((e) => getComputedStyle(e).opacity)).toBe("1");
        await expectContrast(page, time, 4.5, "the gutter time on the hovered own row");
      });

      test("your own DM row's text", async ({ page }) => {
        await page.goto("/");
        await waitForApp(page);
        await selectRoom(page, "Team Chat Room");
        await callRiverTest(page, "appendDmsForPeer", DM_PEER.index, 2);
        expect(await callRiverTest(page, "sendDmToPeer", DM_PEER.index, "my own reply")).toBe("Sent");
        await openThread(page, DM_PEER, "my own reply");
        const own = page.locator('[data-testid="dm-row"][data-message-self="true"]').filter({ hasText: "my own reply" });
        await page.mouse.move(0, 0);
        await expectContrast(page, own.getByTestId("dm-body").locator("p").first(), 4.5, "own DM text");
        const header = own.getByTestId("dm-group-header").locator("span");
        await expectContrast(page, header.first(), 4.5, "own DM name");
        await expectContrast(page, header.last(), 4.5, "own DM time");
        await expect(dm(page, "my own reply")).toHaveCount(1);
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
