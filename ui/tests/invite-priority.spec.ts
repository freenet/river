import { test, expect, Page } from "@playwright/test";

// #566 — the members panel's invite priority.
//
// Background: "Invite Member" minted an invite link and was the panel's
// only accent-filled action, so it was what everyone clicked. That link
// contains a private key and is good for exactly ONE person; in the field
// people share one link with several people, and everyone who used it then
// holds the same identity, so none of them work. The DM route has no such
// failure mode — the recipient gets an Accept button inside a DM thread and
// nothing is copied anywhere — but it lived behind a member's card in a
// DIFFERENT room, three clicks deep, so nobody found it.
//
// This file pins the resulting arrangement: "Share Invite" is the primary
// action and opens a contact picker for the CURRENT room; the link flow is
// still reachable, but secondary. `members.rs`'s
// `share_invite_is_the_primary_invite_action_and_the_link_flow_is_secondary`
// pins the same thing at the source level; this one checks the rendered
// result a user actually meets.
//
// Not covered here: the send itself. Under `no-sync` the chat delegate is
// not running, so the outbound-DM save fails — the same limitation
// `invite-via-dm-picker.spec.ts` documents. Everything up to and including
// "Send is armed" is covered.

const ROOM_NAME = "Public Discussion Room";

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function openRoom(page: Page, name: string = ROOM_NAME) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }
  const roomBtn = page.getByRole("button", { name });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name })).toBeVisible({
    timeout: 5_000,
  });
}

// Read the picker's contact rows. The row carries `data-person` /
// `data-room` precisely so this does not have to parse the accessible name:
// that string also carries the impersonation warning when there is one, and
// prose is expected to change.
async function readContactRows(
  page: Page,
): Promise<{ person: string; room: string }[]> {
  const rows = page.getByTestId("invite-contact-row");
  const count = await rows.count();
  const out: { person: string; room: string }[] = [];
  for (let i = 0; i < count; i++) {
    const person = (await rows.nth(i).getAttribute("data-person")) || "";
    const room = (await rows.nth(i).getAttribute("data-room")) || "";
    out.push({ person, room });
  }
  return out;
}

test.describe("Invite priority: DM route primary, link route secondary", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("Share Invite sits above Invite by link in the members panel", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoom(page);

    const share = page.getByTestId("share-invite-button");
    const link = page.getByTestId("invite-member-button");
    await expect(share).toBeVisible();
    await expect(link).toBeVisible();

    const shareBox = await share.boundingBox();
    const linkBox = await link.boundingBox();
    expect(shareBox).not.toBeNull();
    expect(linkBox).not.toBeNull();
    // Primary above secondary. A user reads the top control as the thing
    // to do, which is the whole point of the reordering.
    expect(shareBox!.y).toBeLessThan(linkBox!.y);

    // ...and it must LOOK primary. The link button used to carry the
    // accent fill; if it ever does again, minting a one-person bearer
    // credential reads as the default way to invite people.
    const shareBg = await share.evaluate(
      (el) => getComputedStyle(el).backgroundColor,
    );
    const linkBg = await link.evaluate(
      (el) => getComputedStyle(el).backgroundColor,
    );
    expect(shareBg).not.toEqual(linkBg);
  });

  test("Share Invite opens a contact picker for the current room", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoom(page);

    await page.getByTestId("share-invite-button").click();
    const modal = page.getByTestId("invite-contact-picker-modal");
    await expect(modal).toBeVisible({ timeout: 5_000 });
    // The header names the room the invitation is FOR — the direction that
    // distinguishes this picker from the member-card one, which invites a
    // known person to some OTHER room.
    await expect(modal).toContainText(`Invite someone to ${ROOM_NAME}`);

    // Candidates come from the user's OTHER rooms: you cannot invite
    // somebody to a room they are already in.
    const rows = await readContactRows(page);
    expect(rows.length).toBeGreaterThan(0);
    for (const row of rows) {
      expect(row.room).not.toEqual(ROOM_NAME);
    }
    expect(rows.some((r) => r.room === "Team Chat Room")).toBeTruthy();
  });

  test("Send is armed only once a contact is picked", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoom(page);

    await page.getByTestId("share-invite-button").click();
    await expect(
      page.getByTestId("invite-contact-picker-modal"),
    ).toBeVisible({ timeout: 5_000 });

    const send = page.getByTestId("invite-contact-picker-send-button");
    await expect(send).toBeDisabled();

    const firstRow = page.getByTestId("invite-contact-row").first();
    await expect(firstRow).toHaveAttribute("aria-pressed", "false");
    await firstRow.click();
    await expect(firstRow).toHaveAttribute("aria-pressed", "true");
    await expect(send).toBeEnabled();
  });

  test("the contact picker filters by person and by room", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoom(page);

    await page.getByTestId("share-invite-button").click();
    await expect(
      page.getByTestId("invite-contact-picker-modal"),
    ).toBeVisible({ timeout: 5_000 });

    const all = await readContactRows(page);
    expect(all.length).toBeGreaterThan(0);

    // Filtering by ROOM is deliberate: "someone from the team room" is how
    // people remember a contact whose name they don't recall.
    const search = page.getByTestId("invite-contact-picker-search");
    await search.fill("Team Chat");
    await expect
      .poll(async () => (await readContactRows(page)).length)
      .toBeGreaterThan(0);
    for (const row of await readContactRows(page)) {
      expect(row.room).toEqual("Team Chat Room");
    }

    // Filtering by PERSON narrows to that person.
    const target = all[0].person;
    await search.fill(target);
    await expect
      .poll(async () => (await readContactRows(page)).length)
      .toBeGreaterThan(0);
    for (const row of await readContactRows(page)) {
      expect(row.person.toLowerCase()).toContain(target.toLowerCase());
    }

    // A query that matches nobody says so, rather than rendering an
    // unexplained empty list.
    await search.fill("zzzzz-nobody-zzzzz");
    await expect(
      page.getByTestId("invite-contact-picker-no-matches"),
    ).toBeVisible();
    expect(await readContactRows(page)).toHaveLength(0);
  });

  // The picker is now the primary invite action, and it lists members of
  // rooms the user may never have opened — so it is the one surface where a
  // name-imitation could otherwise render clean. The example fixture seeds
  // two confusable names (#494), which is what makes this reachable.
  test("a confusable name carries the impersonation warning in the picker", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoom(page);

    await page.getByTestId("share-invite-button").click();
    await expect(page.getByTestId("invite-contact-picker-modal")).toBeVisible({
      timeout: 5_000,
    });

    const warned = page.getByTestId("invite-contact-impersonation-warning");
    expect(await warned.count()).toBeGreaterThan(0);
    // The badge must explain itself. `title=` does not fire on touch, but it
    // is the same affordance the member list uses, and the row's accessible
    // name repeats the warning for screen readers (asserted below).
    const tip = await warned.first().getAttribute("title");
    expect(tip).toBeTruthy();

    // An explicit aria-label on the button REPLACES its content for naming,
    // so a badge rendered inside is invisible to a screen reader unless the
    // label says it too.
    const warnedRow = page
      .getByTestId("invite-contact-row")
      .filter({ has: page.getByTestId("invite-contact-impersonation-warning") })
      .first();
    await expect(warnedRow).toHaveAttribute(
      "aria-label",
      /visually identical/i,
    );
    // The remedy the tooltip names — the member ID — must be reachable.
    await expect(warnedRow).toHaveAttribute("title", /^Member ID: /);
  });

  test("closing the picker leaves no picker behind", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoom(page);

    await page.getByTestId("share-invite-button").click();
    await expect(
      page.getByTestId("invite-contact-picker-modal"),
    ).toBeVisible({ timeout: 5_000 });
    await page.getByTestId("invite-contact-picker-close-button").click();
    await expect(
      page.getByTestId("invite-contact-picker-modal"),
    ).toHaveCount(0);
  });

  test("Invite by link still opens the link modal", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoom(page);

    // Demoted, not removed: a link is still the only way to invite
    // somebody who is not on River yet.
    await page.getByTestId("invite-member-button").click();
    await expect(page.getByTestId("invite-member-modal")).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.getByTestId("invite-link-input")).not.toHaveValue("", {
      timeout: 10_000,
    });
  });
});
