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
  // (members.rs:341). Example-data populates them with random names each
  // app load, so we can't rely on a fixed index for anyone but the local
  // user — who is pinned to index 0 since freenet/river#584, though this
  // scan deliberately does not depend on that. Pick the first member
  // whose text doesn't include "(You)".
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

  // The in-thread "Share invite" entry point. The Rust side can only pin
  // that certain strings exist in the source — it cannot see whether the
  // button renders, whether it opens the picker, or whether its
  // accessible name collides with the selectors the other specs use
  // (the picker overlays this modal, which stays mounted underneath).
  test("share-invite button opens the picker from inside the thread", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const opened = await openDmThreadModal(page);
    if (!opened) {
      test.skip(true, "no DM entry point available in example data");
      return;
    }

    const shareInvite = page.getByTestId("dm-thread-share-invite-button");
    await expect(shareInvite).toBeVisible({ timeout: 5_000 });

    // The new control must not be picked up by the selectors the other
    // specs use. Asserted on its own accessible name rather than on a
    // count: the app already renders two buttons named "Send" (the room
    // composer's and this modal's), which is why the test above reaches
    // for `.last()` — a count assertion here would encode that incidental
    // number and fail for reasons unrelated to this control.
    const inviteName = await shareInvite.getAttribute("aria-label");
    expect(inviteName).toMatch(/^Share invite with /);
    for (const selector of [/^send$/i, /^send invite$/i, /share an invite/i]) {
      expect(inviteName ?? "").not.toMatch(selector);
    }

    await shareInvite.click();

    // The picker paints over the still-mounted thread modal.
    await expect(
      page.getByRole("button", { name: /close picker/i }),
    ).toBeVisible({ timeout: 5_000 });
    await expect(page.getByText(/invite .+ to another room/i)).toBeVisible();
  });

  // Sending is the modal's primary purpose, so Send keeps the only filled
  // accent treatment and the invite control sits on the secondary tier.
  // Asserted on the rendered DOM rather than on the source, so it survives
  // a class list being moved or recomputed.
  test("share-invite is styled below Send", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const opened = await openDmThreadModal(page);
    if (!opened) {
      test.skip(true, "no DM entry point available in example data");
      return;
    }

    const shareInvite = page.getByTestId("dm-thread-share-invite-button");
    await expect(shareInvite).toBeVisible({ timeout: 5_000 });

    const send = page.getByRole("button", { name: /^send$/i }).last();
    const inviteClass = (await shareInvite.getAttribute("class")) || "";
    const sendClass = (await send.getAttribute("class")) || "";

    // Send keeps the filled accent treatment; the invite link carries no
    // background at all.
    expect(sendClass).toContain("bg-accent");
    expect(inviteClass).not.toContain("bg-accent");
    expect(inviteClass).not.toContain("bg-surface");

    // Measured, not assumed: the invite link must actually render smaller
    // than Send. A class-name check alone would pass even if the utilities
    // stopped resolving (e.g. a Tailwind `@source` regression), and it is
    // the rendered size Ian is reacting to.
    const inviteBox = await shareInvite.boundingBox();
    const sendBox = await send.boundingBox();
    expect(inviteBox).not.toBeNull();
    expect(sendBox).not.toBeNull();
    expect(inviteBox!.height).toBeLessThan(sendBox!.height);

    // …and it must not be sitting in the composer row beside Send, where it
    // would take width from the message box. Same row ⇒ vertically overlapping.
    const composer = page.locator(
      'textarea[placeholder="Type a direct message..."]',
    );
    const composerBox = await composer.boundingBox();
    expect(composerBox).not.toBeNull();
    expect(inviteBox!.y).toBeGreaterThan(composerBox!.y + composerBox!.height);

    // Sharing an invite and deleting someone's messages are opposites, so
    // they must not render identically. Compared as COMPUTED colour rather
    // than class names: the point is what a user actually sees, and a
    // hover-only difference is invisible on touch.
    const deleteLink = page.getByRole("button", {
      name: /delete their messages/i,
    });
    await expect(deleteLink).toBeVisible();
    const colourOf = (loc: typeof deleteLink) =>
      loc.evaluate((el) => getComputedStyle(el).color);
    expect(await colourOf(shareInvite)).not.toBe(await colourOf(deleteLink));
  });
});
