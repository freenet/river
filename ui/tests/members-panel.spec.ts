import { test, expect } from "@playwright/test";
import { memberRows, openRoomWithComposer, resolveColor, selectRoom, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// The member panel: collapsible on desktop from the header's members toggle
// (open by default, for the session only), and its rows show an avatar, the
// name and the markers as quiet line icons.

test.describe("Member panel on desktop", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the header toggle hides and shows it, pressed while it shows", async ({ page }) => {
    await openRoomWithComposer(page);
    const panel = page.getByTestId("members-rail");
    const toggle = page.getByTestId("members-panel-toggle");
    const scroller = page.locator("#chat-scroll-container");
    await expect(panel).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-pressed", "true");
    await expect(toggle).toHaveAttribute("aria-controls", "members-rail");
    const count = await memberRows(page).count();
    await expect(toggle).toHaveAccessibleName(`Members, ${count}`);
    await expect(toggle).toContainText(String(count));
    const narrow = (await scroller.boundingBox())!.width;

    await toggle.click();
    await expect(panel).toBeHidden();
    await expect(toggle).toHaveAttribute("aria-pressed", "false");
    // The conversation takes the space.
    await expect.poll(async () => (await scroller.boundingBox())!.width).toBeGreaterThan(narrow + 200);

    await toggle.click();
    await expect(panel).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-pressed", "true");
  });

  test("the choice lasts the session only: a reload opens it again", async ({ page }) => {
    await openRoomWithComposer(page);
    await page.getByTestId("members-panel-toggle").click();
    await expect(page.getByTestId("members-rail")).toBeHidden();
    // Switching rooms keeps it closed.
    await selectRoom(page, "Team Chat Room");
    await expect(page.getByTestId("members-rail")).toBeHidden();
    await page.reload();
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await expect(page.getByTestId("members-rail")).toBeVisible();
    expect(await page.evaluate(() => {
      try {
        return Object.keys(window.localStorage).filter((k) => /member/i.test(k));
      } catch {
        return [];
      }
    }), "no member-panel state in localStorage").toEqual([]);
  });

  test("a row is an avatar, the name and quiet markers; an ordinary name fits", async ({ page }) => {
    await openRoomWithComposer(page);
    // A fixed test member with an ordinary-length name.
    await callRiverTest(page, "insertMessageBeforeLast", "a member to list");
    const row = memberRows(page).filter({ hasText: "Test Sender Insert" });
    await expect(row).toHaveCount(1);
    const m = await row.evaluate((el) => {
      const name = el.querySelector('[data-testid="member-name"]') as HTMLElement;
      const avatar = el.querySelector(".msg-avatar")!;
      return {
        clipped: name.scrollWidth > name.clientWidth,
        avatar: avatar.getBoundingClientRect().width,
        avatarHidden: avatar.getAttribute("aria-hidden"),
        initials: avatar.textContent,
      };
    });
    expect(m.clipped, "an ordinary name is not truncated").toBe(false);
    expect(m.avatar).toBeCloseTo(28, 0);
    expect(m.avatarHidden).toBe("true");
    expect(m.initials).toBe("TS");

    // Your own row: a muted "you", not a glyph.
    const self = page.getByTestId("member-list-self");
    await expect(self).toHaveText("you");
    await expect(self).toHaveAttribute("title", "You");
    // The owner's crown is a line icon in the muted colour, labelled as before.
    const owner = page.getByTestId("member-list-owner");
    await expect(owner).toHaveAttribute("aria-label", "Room Owner");
    await expect(owner.locator("svg")).toHaveCount(1);
    const [ownerColour, muted] = await Promise.all([
      owner.locator("span").first().evaluate((el) => getComputedStyle(el).color),
      resolveColor(page, "var(--color-text-muted)"),
    ]);
    expect(await resolveColor(page, ownerColour)).toEqual(muted);
  });

  test("Invite someone is outlined, not a solid accent fill", async ({ page }) => {
    await openRoomWithComposer(page);
    const invite = page.getByTestId("invite-member-button");
    await expect(invite).toHaveText(/Invite someone/);
    const [bg, accent] = await Promise.all([
      invite.evaluate((el) => getComputedStyle(el).backgroundColor),
      resolveColor(page, "var(--color-accent)"),
    ]);
    expect(await resolveColor(page, bg)).not.toEqual(accent);
    await expect(page.getByTestId("export-id-button")).toBeVisible();
  });
});

test.describe("Member panel on a phone", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("the header's members button opens the Members view, as before", async ({ page }) => {
    await openRoomWithComposer(page);
    await expect(page.getByTestId("members-panel-toggle")).toBeHidden();
    await page.getByTestId("header-members-button").click();
    await expect(page.getByTestId("members-rail")).toBeVisible();
    await expect(page.getByTestId("message-composer")).toBeHidden();
    await page.getByTestId("members-back-to-chat-button").click();
    await expect(page.getByTestId("message-composer")).toBeVisible();
  });
});
