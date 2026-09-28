import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";
import { callRiverTest } from "./river-test";

// Copy test for the invite-member modal's guidance blocks.
//
// The modal used to warn ONLY about the link being single-use, never
// mentioning that River can send an invitation directly in a DM (#252,
// #457) — which is the recommended flow: ask the person first, then use
// "Share invite" from their member card in a room you already share, so
// no bearer credential travels through an outside channel.
//
// These assertions pin BOTH blocks and their order, so a future refactor
// of this modal can't silently drop the recommendation and leave
// copy-the-link as the only documented path.

// A room where the test user is a member, so "Invite Member" can generate
// an invitation (matches the portable-invite-code spec).
const ROOM_NAME = "Public Discussion Room";

async function openInviteModal(page: Page) {
  await selectRoom(page, ROOM_NAME);
  await page.getByTestId("invite-member-button").click();
  await expect(page.getByTestId("invite-member-modal")).toBeVisible({
    timeout: 5_000,
  });
  // The invitation is generated asynchronously (delegate signing with a
  // local fallback); the guidance blocks render alongside it.
  await expect(page.getByTestId("invite-link-input")).not.toHaveValue("", {
    timeout: 10_000,
  });
}

test.describe("Invite-member modal guidance copy", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("recommends a DM invite above the one-person-only link warning", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);

    const rec = page.getByTestId("invite-dm-recommendation");
    await expect(rec).toBeVisible();
    await expect(rec).toContainText(/in a DM/i);
    await expect(rec).toContainText(/Share invite/);

    const warning = page.getByTestId("invite-share-warning");
    await expect(warning).toBeVisible();
    await expect(warning).toContainText(/one person only/i);
    await expect(warning).toContainText(/New Invitation/);

    // Recommended path first — the fallback warning sits below it.
    const recBox = await rec.boundingBox();
    const warnBox = await warning.boundingBox();
    expect(recBox).not.toBeNull();
    expect(warnBox).not.toBeNull();
    expect(recBox!.y).toBeLessThan(warnBox!.y);
  });
});

test.describe("Invite-member modal lifecycle", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  // Records every needle that ever appears in the page text, however briefly.
  async function watchForText(page: Page, needles: string[]) {
    await page.evaluate((needles) => {
      const seen: string[] = ((window as any).__seenText = []);
      new MutationObserver(() => {
        const text = document.body.innerText;
        for (const n of needles) if (text.includes(n) && !seen.includes(n)) seen.push(n);
      }).observe(document.body, { childList: true, subtree: true, characterData: true });
    }, needles);
    return () => page.evaluate(() => (window as any).__seenText as string[]);
  }

  // The invitation resource used to run while the modal was closed, so opening it first showed that run's error.
  test("opening it never shows a stale error", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    const seen = await watchForText(page, ["Modal closed", "Try Again"]);
    await openInviteModal(page); // waits for the link
    expect(await seen()).toEqual([]);
  });

  test("New Invitation replaces the link", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);
    const link = page.getByTestId("invite-link-input");
    const first = await link.inputValue();
    await page.getByTestId("invite-new-invitation-button").click();
    await expect(link).not.toHaveValue(first, { timeout: 10_000 });
  });

  test("an arriving message does not replace the link", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);
    const link = page.getByTestId("invite-link-input");
    const first = await link.inputValue();
    await callRiverTest(page, "appendMessage", "arrived while inviting");
    await expect(page.getByText("arrived while inviting")).toBeAttached({ timeout: 5_000 });
    // Fixed hold: proving the link does NOT change needs a wait; a local re-mint lands well inside it.
    await page.waitForTimeout(600);
    await expect(link).toHaveValue(first);
  });

  // A notification click switches CURRENT_ROOM with this modal still open. The
  // modal closes, and until it does the message never names the new room over
  // the old room's link.
  test("a room switch closes the modal without naming the new room", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);
    const link = page.getByTestId("invite-link-input");
    const message = page.getByTestId("invite-message-text");
    const first = await link.inputValue();
    await expect(message).toContainText(ROOM_NAME);

    // Every (message, link) pair ever rendered, however briefly.
    await page.evaluate(() => {
      const seen: { message: string; link: string }[] = ((window as any).__invitePairs = []);
      const record = () => {
        const m = document.querySelector('[data-testid="invite-message-text"]');
        const l = document.querySelector('[data-testid="invite-link-input"]') as HTMLInputElement | null;
        if (m && l) seen.push({ message: m.textContent ?? "", link: l.value });
      };
      new MutationObserver(record).observe(document.body, {
        childList: true,
        subtree: true,
        characterData: true,
        attributes: true,
      });
    });

    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await expect(page.getByTestId("invite-member-modal")).toHaveCount(0, { timeout: 5_000 });
    // The switch really happened: the header now shows the new room.
    await expect(page.getByTestId("room-title-button")).toContainText("Team Chat Room");

    const pairs = (await page.evaluate(() => (window as any).__invitePairs)) as {
      message: string;
      link: string;
    }[];
    const mismatched = pairs.filter((p) => p.message.includes("Team Chat Room"));
    expect(mismatched, "the invite message named the room switched to").toEqual([]);
    expect(pairs.every((p) => p.link === first), "the link was re-minted").toBe(true);
  });
});
