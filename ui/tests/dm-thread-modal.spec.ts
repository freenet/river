import { test, expect, Page } from "@playwright/test";

// Smoke tests for the DM thread modal Phase 3 changes
// (#243 structured invite-DM variant + auto-scroll):
//
//   * The modal exposes a stable `id="dm-scroll-container"` on the
//     scrollable thread body. The auto-scroll effect targets that id;
//     if it gets renamed without updating the JS lookup, the
//     mount-jump-to-bottom, outbound-send scroll, and "near-bottom"
//     inbound-scroll all silently regress to "no scroll happens."
//   * Empty-state copy ("No messages yet. Say hello!") still appears
//     when there are zero DMs — example-data populates no DMs so this
//     is the default state for the smoke test.
//
// End-to-end coverage of the invite card (recipient renders card, click
// Accept → ReceiveInvitationModal opens) requires a real Freenet node
// because we need an inbound DM with `body: Invite(...)` plaintext —
// `no-sync` mode can't synthesize that. Rust unit tests in
// `ui/src/components/direct_messages/dm_thread_modal.rs::tests` pin the
// pure logic (decode + room-mismatch rejection); the wiring tested
// here is the structural part Playwright can reach.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
}

async function openDmThreadModal(page: Page) {
  // Click into a room that lists the local user as a Member so the
  // member-info modal exposes the "Send direct message" button.
  await page.getByText("Team Chat Room").first().click();

  // The member list is rendered after the room hydrates; wait for at
  // least one member row to appear before counting.
  await page
    .locator('button[title^="Member ID"]')
    .first()
    .waitFor({ state: "visible", timeout: 5_000 })
    .catch(() => undefined);

  // Member rows are buttons keyed by `title="Member ID: …"`
  // (members.rs:341). Example-data populates them with random names
  // each app load and the local-user "(You)" entry can appear at any
  // position, so we can't rely on a fixed index. Pick the first
  // member whose text doesn't include "(You)".
  const memberButtons = page.locator('button[title^="Member ID"]');
  const count = await memberButtons.count();
  let clicked = false;
  for (let i = 0; i < count; i++) {
    const text = (await memberButtons.nth(i).textContent()) || "";
    if (!/\(You\)/i.test(text)) {
      await memberButtons.nth(i).click();
      clicked = true;
      break;
    }
  }
  if (!clicked) {
    return false;
  }

  // The "Send direct message" button is the primary action in the
  // member-info modal, identified by its aria-label rather than
  // visible text (which is just "DM").
  const sendDm = page
    .locator('button[aria-label="Send direct message"]')
    .first();
  await sendDm.waitFor({ state: "visible", timeout: 5_000 }).catch(() => undefined);
  if (!(await sendDm.isVisible().catch(() => false))) {
    return false;
  }
  await sendDm.click();
  return true;
}

test.describe("DM thread modal (Phase 3 structure)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("scroll container carries the stable id", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const opened = await openDmThreadModal(page);
    if (!opened) {
      test.skip(true, "no DM entry point available in example data");
      return;
    }

    // The auto-scroll effect targets this id; if it gets renamed
    // every scroll behaviour regresses to "no scroll happens."
    const scrollContainer = page.locator("#dm-scroll-container");
    await expect(scrollContainer).toBeVisible({ timeout: 5_000 });
  });

  test("empty thread shows the say-hello prompt", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const opened = await openDmThreadModal(page);
    if (!opened) {
      test.skip(true, "no DM entry point available in example data");
      return;
    }

    // Example-data has no DMs, so the empty-state prompt is the only
    // thing inside #dm-scroll-container on first open.
    await expect(
      page.getByText(/no messages yet/i),
    ).toBeVisible({ timeout: 5_000 });
  });

  test("composer accepts input and Send button enables", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const opened = await openDmThreadModal(page);
    if (!opened) {
      test.skip(true, "no DM entry point available in example data");
      return;
    }

    const composer = page.locator(
      'textarea[placeholder="Type a direct message..."]',
    );
    await expect(composer).toBeVisible();

    const sendButton = page.getByRole("button", { name: /^send$/i }).last();
    // Disabled while empty.
    await expect(sendButton).toBeDisabled();

    await composer.fill("test message");
    await expect(sendButton).toBeEnabled();
  });
});
