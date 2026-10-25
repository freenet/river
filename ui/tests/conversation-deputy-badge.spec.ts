import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// Render coverage for the two halves of the deputy-badge change:
//
//  1. A message author who holds deputy authority relevant to the viewer gets
//     a 🛡 badge next to their name, with a tooltip naming the appointer and
//     saying whether they can ban you. Authors without that authority don't.
//
//  2. That badge cannot be faked by a nickname. Example data deliberately
//     gives the room OWNER a stored nickname ending "🛡👑" — a spoof
//     attempt — and `crate::util::display_name` strips it at render time, so
//     the owner's author line shows no glyph at all.
//
// Example data (ui/src/example_data.rs): in "Team Chat Room" the owner
// deputizes one member (a global moderator, relevant to every viewer) and
// that member is given one guaranteed message so this spec is
// deterministic. Names are random, so the owner and the deputy are found by
// their markers (the OWNER tag, the member panel's deputy shield).
//
// Rust unit tests pin the predicate itself (`deputy_badges_for_viewer`) and
// the sanitiser; this spec pins that the two are actually wired to the DOM.
// The member-list side of the spoof check lives in member-info-deputy-tag.spec.ts.

const BADGE = '[data-testid="message-author-deputy-badge"]';
// The author-name button in a message header.
const AUTHOR_NAME = '#chat-scroll-container [data-testid="message-author-name"]';
// Every glyph River uses as a badge somewhere in the UI. None may appear in a
// rendered author NAME.
const BADGE_GLYPHS = ["🛡", "👑", "⭐", "🔑", "🎪"];

async function openTeamChat(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  // Author lines only render for OTHER people's messages, and the deputy is
  // guaranteed to have posted one, so waiting for the badge also guarantees
  // the conversation has finished rendering.
  await page.locator(BADGE).first().waitFor({ state: "visible", timeout: 15_000 });
}

test.describe("Deputy badge on message authors", { tag: "@chromium-only" }, () => {
  // Fixed desktop viewport so the conversation pane is always in view
  // (mirrors member-info-deputy-tag.spec.ts).
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the deputy's message author line carries the 🛡 badge", async ({
    page,
  }) => {
    await openTeamChat(page);

    // The tooltip names the appointer AND says what it means for the viewer.
    // The deputy here was appointed by the room owner and can ban the viewer.
    await expect(page.locator(BADGE).first()).toHaveAttribute(
      "title",
      "Deputy (appointed by the room owner). Can ban you.",
    );

    // Every badge sits next to the SAME author — the one deputy in this room.
    const badgedNames = await page.locator(BADGE).evaluateAll((badges) =>
      badges.map((b) => {
        const name = b.parentElement?.querySelector(
          '[data-testid="message-author-name"]',
        );
        return (name?.textContent || "").trim();
      }),
    );
    expect(badgedNames.length).toBeGreaterThan(0);
    expect(new Set(badgedNames).size).toBe(1);
    // …and that author is the member the panel marks as the deputy.
    const deputyName = await page
      .getByTestId("member-list")
      .locator('[data-testid^="member-item-"] button')
      .filter({ has: page.getByTestId("member-list-deputy") })
      .getByTestId("member-name")
      .textContent();
    expect(badgedNames[0]).toBe((deputyName ?? "").trim());
  });

  test("a non-deputy author has no badge", async ({ page }) => {
    await openTeamChat(page);

    // The owner also posts in example data, and the owner is nobody's deputy,
    // so their author line must carry no badge.
    const ownerHeader = page
      .getByTestId("message-group-header")
      .filter({ has: page.getByTestId("message-author-owner-tag") })
      .first();
    await expect(ownerHeader).toBeVisible();
    await expect(ownerHeader.locator(BADGE)).toHaveCount(0);
  });

  test("an emoji nickname cannot paint a badge into the author line", async ({
    page,
  }) => {
    await openTeamChat(page);

    // The owner's STORED nickname ends with 🛡👑 (see example_data.rs). None
    // of it may survive into any rendered author name.
    const names = await page.locator(AUTHOR_NAME).allTextContents();
    expect(names.length).toBeGreaterThan(0);
    for (const name of names) {
      for (const glyph of BADGE_GLYPHS) {
        expect(
          name,
          `author name rendered a badge glyph from a nickname: ${JSON.stringify(name)}`,
        ).not.toContain(glyph);
      }
    }
    // Sanity: the owner really is on screen, so the loop above wasn't vacuous.
    const ownerName = await page
      .getByTestId("message-group-header")
      .filter({ has: page.getByTestId("message-author-owner-tag") })
      .first()
      .getByTestId("message-author-name")
      .textContent();
    expect(names.map((n) => n.trim())).toContain((ownerName ?? "").trim());
  });
});
