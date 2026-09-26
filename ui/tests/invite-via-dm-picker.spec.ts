import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";
import { isSelfRowText, memberRows, openMemberInfoForFirstNonSelf, openShareInvitePicker } from "./invite-picker";

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

// Open the member-info modal for the member row at `memberIndex`, click
// "Share an invite via DM", and return the nickname rendered in the
// picker title ("Invite <name> to another room").
async function openPickerAndReadTitle(page: Page, memberIndex: number): Promise<string> {
  await memberRows(page).nth(memberIndex).click();
  await openShareInvitePicker(page);

  const header = page.getByRole("heading", {
    name: /invite .+ to another room/i,
  });
  const headingText = ((await header.textContent()) || "").trim();
  const match = headingText.match(/^Invite (.+) to another room$/);
  expect(match, `picker header didn't match the expected format: "${headingText}"`).not.toBeNull();
  return match![1];
}

// Dismiss the picker, then the member-info modal behind it, leaving the
// member list interactable again.
async function closePickerAndMemberInfo(page: Page) {
  await page.getByRole("button", { name: /close picker/i }).click();
  await expect(
    page.getByRole("heading", { name: /invite .+ to another room/i }),
  ).toHaveCount(0);

  // Dismiss the member-info modal by clicking its backdrop. Target the
  // backdrop element explicitly (not a screen coordinate) and click a
  // corner, clear of the centered modal card. The picker's own backdrop
  // is already gone (asserted above), so the only `bg-black/50` overlay
  // left is the member-info modal's.
  await page
    .locator('div[class*="bg-black/50"]')
    .last()
    .click({ position: { x: 5, y: 5 } });
  await expect(
    page.getByRole("heading", { name: /^Member Info$/ }),
  ).toHaveCount(0);
}

// Read the destination-room names from the picker's candidate rows.
// Each candidate row is a button with aria-label
// "Select room <name> as the invite destination".
async function readCandidateRoomNames(page: Page): Promise<string[]> {
  const rows = page.locator('button[aria-label^="Select room"]');
  const count = await rows.count();
  const names: string[] = [];
  for (let i = 0; i < count; i++) {
    const aria = (await rows.nth(i).getAttribute("aria-label")) || "";
    const m = aria.match(/^Select room (.+) as the invite destination$/);
    if (m) names.push(m[1]);
  }
  return names;
}

// All member rows that are not the local user, with their list index.
async function nonSelfMemberRows(
  page: Page,
): Promise<{ index: number; text: string }[]> {
  const rows = memberRows(page);
  await expect(rows.first(), "the member list rendered").toBeVisible({ timeout: 5_000 });
  const count = await rows.count();
  const result: { index: number; text: string }[] = [];
  for (let i = 0; i < count; i++) {
    const text = ((await rows.nth(i).textContent()) || "").trim();
    if (text && !isSelfRowText(text)) result.push({ index: i, text });
  }
  return result;
}

test.describe("Invite-via-DM picker (structured-Invite variant)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("opens a composer with room dropdown, personal-message field, and Send button", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await openMemberInfoForFirstNonSelf(page);
    await openShareInvitePicker(page);

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

    await openMemberInfoForFirstNonSelf(page);
    await openShareInvitePicker(page);

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
  // the stale cached name. This test opens the picker for two different
  // members in succession and asserts the title tracks the current one.
  test("title tracks the current member when the picker is reopened (regression: Ivvor 2026-05-20)", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await selectRoom(page, "Team Chat Room");

    // Two different non-self member rows — each row is a distinct member.
    const nonSelf = await nonSelfMemberRows(page);
    expect(
      nonSelf.length,
      "Team Chat Room lists at least two members other than the local user",
    ).toBeGreaterThanOrEqual(2);
    const [memberA, memberB] = nonSelf;

    // First invite: open the picker for member A.
    const titleA = await openPickerAndReadTitle(page, memberA.index);
    await closePickerAndMemberInfo(page);

    // Second invite: reopen the picker for member B. Before the fix the
    // title still read member A's name here.
    const titleB = await openPickerAndReadTitle(page, memberB.index);

    // The picker title is the unsealed nickname; a member row renders
    // "<nickname> <badges>", so a row text always starts with that
    // member's own title. These assertions pin each title to the member
    // it was opened for — with the bug, titleB held member A's name and
    // `memberB.text.startsWith(titleB)` was false. (A row text can't
    // start with a *different* member's nickname unless one nickname is a
    // PREFIX of another. Since #494 the fixture deliberately contains two
    // confusable names, but `confusable_variant` substitutes a character
    // rather than appending, so they are the same length and neither is a
    // prefix of the other — this still implicitly proves titleA ≠ titleB.)
    expect(titleA.length).toBeGreaterThan(0);
    expect(memberA.text.startsWith(titleA)).toBeTruthy();
    expect(memberB.text.startsWith(titleB)).toBeTruthy();
  });

  // Regression test for the second half of the same fix: `candidates`
  // was also a `use_memo` capturing `current_room` as a plain value, so
  // reopening the picker from a *different* current room kept showing
  // the previous room's candidate list. This switches the current room
  // between two opens and asserts the candidate list follows.
  test("candidate room list tracks the current room when the picker is reopened", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    // First open: picker launched from "Team Chat Room".
    await selectRoom(page, "Team Chat Room");
    const rowsA = await nonSelfMemberRows(page);
    expect(rowsA.length, "Team Chat Room lists a member other than the local user").toBeGreaterThan(0);
    await openPickerAndReadTitle(page, rowsA[0].index);
    const candidatesFromTeamChat = await readCandidateRoomNames(page);
    await closePickerAndMemberInfo(page);

    // Switch the current room. Member keys are per-room, so the first
    // member row's data-testid changes when the room switches — wait for
    // that before reading the new room's member list (avoids racing the
    // re-render and reading the old room's rows).
    const memberItems = page
      .getByTestId("member-list")
      .locator('[data-testid^="member-item-"]');
    await expect(memberItems.first()).toBeVisible({ timeout: 5_000 });
    const firstMemberTestId = await memberItems.first().getAttribute("data-testid");
    expect(firstMemberTestId).toBeTruthy();
    await selectRoom(page, "Your Private Room");
    await expect(memberItems.first()).not.toHaveAttribute(
      "data-testid",
      firstMemberTestId ?? "",
    );

    const rowsB = await nonSelfMemberRows(page);
    expect(rowsB.length, "Your Private Room lists a member other than the local user").toBeGreaterThan(0);
    await openPickerAndReadTitle(page, rowsB[0].index);
    const candidatesFromPrivateRoom = await readCandidateRoomNames(page);

    // The candidate list excludes the *current* room and includes every
    // other room. So "Team Chat Room" must appear only in the second
    // list, and "Your Private Room" only in the first. With the stale
    // memo, the second list still equalled the first.
    expect(candidatesFromTeamChat).toContain("Your Private Room");
    expect(candidatesFromTeamChat).not.toContain("Team Chat Room");
    expect(candidatesFromPrivateRoom).toContain("Team Chat Room");
    expect(candidatesFromPrivateRoom).not.toContain("Your Private Room");
    // Belt-and-suspenders, independent of the specific room names: the
    // two candidate lists must differ once the current room changed.
    expect(candidatesFromPrivateRoom).not.toEqual(candidatesFromTeamChat);
  });

  // Regression test for the per-open state reset (the `use_effect`): a
  // room selected in one picker session must NOT remain selected when
  // the picker is reopened for a different member. Before the reset, a
  // stale `selected_room` left the Send button armed for a destination
  // the user never picked this session (Codex P2 on PR #291).
  test("a room selected in one session does not stay selected on reopen", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await selectRoom(page, "Team Chat Room");
    const nonSelf = await nonSelfMemberRows(page);
    expect(
      nonSelf.length,
      "Team Chat Room lists at least two members other than the local user",
    ).toBeGreaterThanOrEqual(2);

    // First session: open the picker, pick a candidate room.
    await openPickerAndReadTitle(page, nonSelf[0].index);
    const firstCandidate = page
      .locator('button[aria-label^="Select room"]')
      .first();
    await firstCandidate.click();
    await expect(firstCandidate).toHaveAttribute("aria-pressed", "true");
    await expect(
      page.getByRole("button", { name: /^send invite$/i }),
    ).toBeEnabled();
    await closePickerAndMemberInfo(page);

    // Second session: reopen for a different member. No candidate row
    // should be pre-selected and Send must start disabled.
    await openPickerAndReadTitle(page, nonSelf[1].index);
    await expect(
      page.locator('button[aria-label^="Select room"][aria-pressed="true"]'),
    ).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: /^send invite$/i }),
    ).toBeDisabled();
  });
});
