import { test, expect, Page } from "@playwright/test";

// Render coverage for the two halves of the deputy-badge change:
//
//  1. A message author who holds deputy authority relevant to the viewer gets
//     a 🛡 badge next to their name, with a tooltip naming the appointer and
//     saying whether they can ban you. Authors without that authority don't.
//
//  2. That badge cannot be faked by a nickname. Example data deliberately
//     gives the room OWNER the stored nickname "… (Owner) 🛡👑" — a spoof
//     attempt — and `crate::util::display_name` strips it at render time, so
//     the owner's author line shows no glyph at all.
//
// Example data (ui/src/example_data.rs): in "Team Chat Room" the owner
// deputizes the "(Member)" member (a global moderator, relevant to every
// viewer) and that member is given one guaranteed message so this spec is
// deterministic. The local user is "(You)" and the owner is "(Owner)".
//
// Rust unit tests pin the predicate itself (`deputy_badges_for_viewer`) and
// the sanitiser; this spec pins that the two are actually wired to the DOM.
// The member-list side of the spoof check lives in member-info-deputy-tag.spec.ts.

const BADGE = '[data-testid="message-author-deputy-badge"]';
const WARNING = '[data-testid="message-author-impersonation-warning"]';
// The author-name span in a message header. Scoped to the chat container so it
// cannot pick up the @mention autocomplete's disambiguator, which uses the
// same title prefix.
const AUTHOR_NAME = '#chat-scroll-container span[title^="Member ID"]';
// Every glyph River uses as a badge somewhere in the UI. None may appear in a
// rendered author NAME.
const BADGE_GLYPHS = ["🛡", "👑", "⭐", "🔑", "🎪"];

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
}

async function openTeamChat(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await page.getByText("Team Chat Room").first().click();
  // Author lines only render for OTHER people's messages, and the deputy is
  // guaranteed to have posted one, so waiting for the badge also guarantees
  // the conversation has finished rendering.
  await page.locator(BADGE).first().waitFor({ state: "visible", timeout: 15_000 });
}

test.describe("Deputy badge on message authors", () => {
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
          'span[title^="Member ID"]',
        );
        return (name?.textContent || "").trim();
      }),
    );
    expect(badgedNames.length).toBeGreaterThan(0);
    expect(new Set(badgedNames).size).toBe(1);
    expect(badgedNames[0]).toContain("(Member)");
  });

  test("a non-deputy author has no badge", async ({ page }) => {
    await openTeamChat(page);

    // The owner also posts in example data, and the owner is nobody's deputy,
    // so their author line must carry no badge.
    const ownerHeader = page
      .locator(AUTHOR_NAME)
      .filter({ hasText: "(Owner)" })
      .first();
    await expect(ownerHeader).toBeVisible();
    await expect(ownerHeader.locator("xpath=..").locator(BADGE)).toHaveCount(0);
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
    expect(names.some((n) => n.includes("(Owner)"))).toBe(true);
  });

  test("an impersonator is flagged and the real owner is not", async ({
    page,
  }) => {
    await openTeamChat(page);

    // Example data adds a member whose name differs from the owner's by one
    // Cyrillic character, and gives them a guaranteed message.
    const warning = page.locator(WARNING);
    await expect(warning.first()).toBeVisible({ timeout: 10_000 });

    // The tooltip must name the remedy, not just alarm.
    await expect(warning.first()).toHaveAttribute(
      "title",
      /nearly identical to .*, a moderator, but this is NOT that person\. Look for the shield\./,
    );

    // Exactly one author carries it, and it is NOT the member holding the
    // shield — flagging the genuine article would be worse than nothing.
    const warned = await warning.evaluateAll((els) =>
      els.map((el) => ({
        name: (
          el.parentElement?.querySelector('span[title^="Member ID"]')
            ?.textContent || ""
        ).trim(),
        hasShield: !!el.parentElement?.querySelector(
          '[data-testid="message-author-deputy-badge"]',
        ),
      })),
    );
    expect(warned.length).toBeGreaterThan(0);
    for (const w of warned) {
      expect(
        w.hasShield,
        `a member carrying the shield was also flagged as an impersonator: ${w.name}`,
      ).toBe(false);
    }
    expect(new Set(warned.map((w) => w.name)).size).toBe(1);
  });
});
