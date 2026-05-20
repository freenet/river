import { test, expect, Page } from "@playwright/test";

// Smoke test for the redesigned invite-via-DM picker (PR for #252 v2,
// structured-Invite variant).
//
// What changed:
//
//   * Old picker: list of candidate-room rows; clicking one pasted an
//     invite URL into DM_DRAFT and opened the DM thread modal.
//   * New picker: room dropdown (radio-style rows) + a personal-message
//     textarea + a single "Send invite" button. Sends a structured
//     `DirectMessageBody::Invite` DM directly; no URL paste, no
//     thread-modal hand-off.
//
// We can't easily exercise the end-to-end "send → recipient sees card →
// click Accept → modal opens" path under `no-sync` (the chat delegate
// isn't running, so the outbound-DM save fails and we don't fully verify
// the recipient render path). What we CAN verify here is the picker's
// new visible structure: the header text, the candidate row, the
// personal-message textarea, and the Send button being enabled only
// after a room is selected.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
}

async function openMemberInfo(page: Page) {
  // Select a room that lists the local user as a Member, so the
  // member-info modal's "Share an invite via DM" option appears.
  // Example-data's "Team Chat Room" matches.
  await page.getByText("Team Chat Room").first().click();

  // The member list is rendered after the room hydrates; wait for at
  // least one member row to appear before iterating (otherwise the
  // iterator races the first paint and we get count=0 → skip).
  await page
    .locator('button[title^="Member ID"]')
    .first()
    .waitFor({ state: "visible", timeout: 5_000 })
    .catch(() => undefined);

  // Member rows are buttons with `title="Member ID: …"`
  // (members.rs:341). Example-data populates them with random names
  // each app load and the local-user "You" entry can appear at any
  // position, so we can't rely on a fixed index. Pick the first row
  // whose text does NOT contain "(You)" — i.e. a non-self member.
  const memberButtons = page.locator('button[title^="Member ID"]');
  const count = await memberButtons.count();
  for (let i = 0; i < count; i++) {
    const text = (await memberButtons.nth(i).textContent()) || "";
    if (!/\(You\)/i.test(text)) {
      await memberButtons.nth(i).click();
      return true;
    }
  }
  return false;
}

// Open the member-info modal for the member row at `memberIndex`, click
// "Share an invite via DM", and return the nickname rendered in the
// picker title ("Invite <name> to another room"). Returns null if the
// Share-invite entry point isn't available (observer-only example data).
async function openPickerAndReadTitle(
  page: Page,
  memberIndex: number,
): Promise<string | null> {
  await page.locator('button[title^="Member ID"]').nth(memberIndex).click();

  const shareInvite = page
    .getByRole("button", { name: /share an invite/i })
    .first();
  await shareInvite
    .waitFor({ state: "visible", timeout: 5_000 })
    .catch(() => undefined);
  if (!(await shareInvite.isVisible().catch(() => false))) {
    return null;
  }
  await shareInvite.click();

  const header = page.getByRole("heading", {
    name: /invite .+ to another room/i,
  });
  await expect(header).toBeVisible({ timeout: 5_000 });

  const headingText = ((await header.textContent()) || "").trim();
  const match = headingText.match(/^Invite (.+) to another room$/);
  return match ? match[1] : null;
}

// Dismiss the picker, then the member-info modal behind it, leaving the
// member list interactable again.
async function closePickerAndMemberInfo(page: Page) {
  await page.getByRole("button", { name: /close picker/i }).click();
  await expect(
    page.getByRole("heading", { name: /invite .+ to another room/i }),
  ).toHaveCount(0);

  // The member-info modal closes when its backdrop is clicked; the
  // top-left corner is well clear of the centered modal card.
  await page.mouse.click(5, 5);
  await expect(
    page.getByRole("heading", { name: /^Member Info$/ }),
  ).toHaveCount(0);
}

test.describe("Invite-via-DM picker (structured-Invite variant)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("opens a composer with room dropdown, personal-message field, and Send button", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const opened = await openMemberInfo(page);
    if (!opened) {
      test.skip(true, "example-data has no non-self/owner member to open");
      return;
    }

    // The member-info modal contains a "Share an invite via DM…" entry
    // (the exact label was added in #260; keep the substring match
    // resilient to minor wording tweaks).
    const shareInvite = page
      .getByRole("button", { name: /share an invite/i })
      .first();
    // The member-info modal renders asynchronously after the member-
    // row click; wait briefly for the Share button to materialise
    // before deciding whether to skip.
    await shareInvite
      .waitFor({ state: "visible", timeout: 5_000 })
      .catch(() => undefined);

    // Skip the test cleanly if example data places the local user in
    // fewer than 2 rooms — the picker requires at least one other room
    // to be a viable invite target.
    if (!(await shareInvite.isVisible().catch(() => false))) {
      test.skip(true, "no 'Share an invite via DM' entry point — example data may be observer-only");
      return;
    }

    await shareInvite.click();

    // Picker header should appear. Title format: "Invite <nickname> to
    // another room".
    const header = page.getByRole("heading", {
      name: /invite .+ to another room/i,
    });
    await expect(header).toBeVisible({ timeout: 5_000 });

    // Personal-message textarea is present.
    const textarea = page.locator("textarea").first();
    await expect(textarea).toBeVisible();

    // The Send button starts disabled until a room is picked.
    const sendButton = page.getByRole("button", { name: /^send invite$/i });
    await expect(sendButton).toBeVisible();
    await expect(sendButton).toBeDisabled();

    // Selecting a candidate row enables the Send button. Candidate rows
    // carry aria-pressed; before selection none are pressed.
    const candidateRow = page
      .locator('button[aria-label^="Select room"]')
      .first();
    await expect(candidateRow).toHaveAttribute("aria-pressed", "false");

    await candidateRow.click();
    await expect(candidateRow).toHaveAttribute("aria-pressed", "true");
    await expect(sendButton).toBeEnabled();

    // Typing in the personal-message textarea is reflected.
    await textarea.fill("Want to join us?");
    await expect(textarea).toHaveValue("Want to join us?");
  });

  test("close button dismisses the picker", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const opened = await openMemberInfo(page);
    if (!opened) {
      test.skip(true, "example-data has no non-self/owner member to open");
      return;
    }

    const shareInvite = page
      .getByRole("button", { name: /share an invite/i })
      .first();
    // The member-info modal renders asynchronously after the member-
    // row click; wait briefly for the Share button to materialise
    // before deciding whether to skip.
    await shareInvite
      .waitFor({ state: "visible", timeout: 5_000 })
      .catch(() => undefined);
    if (!(await shareInvite.isVisible().catch(() => false))) {
      test.skip(true, "no 'Share an invite via DM' entry point");
      return;
    }

    await shareInvite.click();
    const closeButton = page.getByRole("button", { name: /close picker/i });
    await expect(closeButton).toBeVisible();
    await closeButton.click();

    // After close the picker header is gone.
    await expect(
      page.getByRole("heading", { name: /invite .+ to another room/i }),
    ).toHaveCount(0);
  });

  // Regression test for Ivvor's 2026-05-20 report: inviting several
  // members one after another via "Share invite" showed the *previous*
  // invitee's name in the "Invite <X> to another room" title. Root
  // cause: the picker's `peer_label` was a `use_memo` that only
  // subscribed to ROOMS, so reopening it for a different peer returned
  // the stale cached name. This test opens the picker for two distinct
  // members in succession and asserts the title tracks the current one.
  test("title tracks the current member when the picker is reopened (regression: Ivvor 2026-05-20)", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await page.getByText("Team Chat Room").first().click();
    await page
      .locator('button[title^="Member ID"]')
      .first()
      .waitFor({ state: "visible", timeout: 5_000 })
      .catch(() => undefined);

    // Collect non-self member rows; we need two with distinct display
    // text so a stale-vs-fresh title is unambiguous.
    const memberButtons = page.locator('button[title^="Member ID"]');
    const count = await memberButtons.count();
    const nonSelf: { index: number; text: string }[] = [];
    for (let i = 0; i < count; i++) {
      const text = ((await memberButtons.nth(i).textContent()) || "").trim();
      if (text && !/\(You\)/i.test(text)) {
        nonSelf.push({ index: i, text });
      }
    }
    const distinct = nonSelf.filter(
      (m, idx) => nonSelf.findIndex((n) => n.text === m.text) === idx,
    );
    if (distinct.length < 2) {
      test.skip(
        true,
        "example data lacks two distinctly-named non-self members",
      );
      return;
    }
    const [memberA, memberB] = distinct;

    // First invite: open the picker for member A.
    const titleA = await openPickerAndReadTitle(page, memberA.index);
    if (titleA === null) {
      test.skip(
        true,
        "no 'Share an invite via DM' entry point — observer-only example data",
      );
      return;
    }
    expect(titleA.length).toBeGreaterThan(0);
    // The member row text is "<nickname> <badges>"; the picker title is
    // exactly the nickname, so the row text starts with the title.
    expect(memberA.text.startsWith(titleA)).toBeTruthy();

    await closePickerAndMemberInfo(page);

    // Second invite: reopen the picker for member B. Before the fix the
    // title still read member A's name here.
    const titleB = await openPickerAndReadTitle(page, memberB.index);
    expect(titleB).not.toBeNull();
    expect(memberB.text.startsWith(titleB!)).toBeTruthy();
    expect(titleB).not.toBe(titleA);
  });
});
