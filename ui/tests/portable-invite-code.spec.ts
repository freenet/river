import { test, expect, Page } from "@playwright/test";

// Feature test for issue #381: portable invite codes.
//
// The invite-member modal must expose a bare, host-independent invite CODE
// (in addition to the existing host-baked link), and the room list must offer
// an "Enter Invite Code" affordance that accepts a pasted code and opens the
// normal invitation flow. This lets a user on a non-standard host (e.g.
// try.freenet.org) join without hand-editing the host out of an invite link.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

// A room where the test user is a member, so the "Invite Member" affordance
// can generate an invitation (matches the copy-clipboard-feedback spec).
const ROOM_NAME = "Public Discussion Room";

async function selectRoom(page: Page) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }
  const roomBtn = page.getByRole("button", { name: ROOM_NAME });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(
    page.getByRole("heading", { name: ROOM_NAME })
  ).toBeVisible({ timeout: 5_000 });
}

async function openInviteModalAndReadCode(page: Page): Promise<string> {
  await page.getByTestId("invite-member-button").click();
  await expect(page.getByTestId("invite-member-modal")).toBeVisible({
    timeout: 5_000,
  });

  const codeInput = page.getByTestId("invite-code-input");
  await expect(codeInput).toBeVisible({ timeout: 5_000 });
  // The invitation is generated asynchronously (delegate signing with a local
  // fallback); wait for the field to be populated.
  await expect(codeInput).not.toHaveValue("", { timeout: 10_000 });
  return await codeInput.inputValue();
}

test.describe("Portable invite codes (issue #381)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("invite modal exposes a portable code with copy feedback", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page);

    const code = await openInviteModalAndReadCode(page);
    // A base58 invite code is a non-trivial string; sanity-check it looks real.
    expect(code.length).toBeGreaterThan(20);

    const copyButton = page.getByTestId("invite-copy-code-button");
    await expect(copyButton).toBeVisible();
    await expect(copyButton).toHaveText(/Copy Code/);
    await copyButton.click();
    await expect(copyButton).toHaveText(/Copied!/, { timeout: 2_000 });
  });

  test("a generated code pasted into 'Enter Invite Code' opens the invitation modal", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page);

    // Create side: grab a real portable code.
    const code = await openInviteModalAndReadCode(page);
    // Close the invite modal.
    await page.getByTestId("invite-member-close-button").click();
    await expect(page.getByTestId("invite-member-modal")).toHaveCount(0);

    // Receive side: paste the code and submit.
    await page.getByTestId("join-with-code-button").click();
    await expect(page.getByTestId("join-with-code-modal")).toBeVisible({
      timeout: 5_000,
    });
    await page.getByTestId("join-with-code-input").fill(code);
    await page.getByTestId("join-with-code-submit-button").click();

    // The shared accept flow must surface the receive-invitation modal. The
    // code targets a room this user already belongs to, so the modal opens on
    // its "already a member" branch — the point is that the paste path decoded
    // the code and routed into the normal flow.
    await expect(page.getByTestId("receive-invitation-modal")).toBeVisible({
      timeout: 5_000,
    });
  });

  test("an unparseable code shows an inline error and does not open the invitation modal", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await page.getByTestId("join-with-code-button").click();
    await expect(page.getByTestId("join-with-code-modal")).toBeVisible({
      timeout: 5_000,
    });
    await page
      .getByTestId("join-with-code-input")
      .fill("this is definitely not a valid invite code !!!");
    await page.getByTestId("join-with-code-submit-button").click();

    await expect(
      page.getByText(/doesn't look like a valid invite code/i)
    ).toBeVisible({ timeout: 3_000 });
    // No invitation modal should appear for garbage input.
    await expect(page.getByTestId("receive-invitation-modal")).toHaveCount(0);
  });
});
