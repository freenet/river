import { test, expect, Page } from "@playwright/test";

// Render coverage for freenet/river#489: the ⚠ impersonation warning.
//
// Example data (ui/src/example_data.rs) seeds an IMPOSTOR — a plain member
// whose nickname is derived from the owner-appointed deputy's so the two fold
// to the same skeleton. `every_example_room_shows_the_impersonation_warning`
// pins in Rust that the fixture actually reaches that state; this spec pins
// that the badge reaches the DOM, which a source-grep cannot catch.
//
// Two things here are structurally beyond a unit test:
//
//  * **Visible explanatory text in the modal.** `title=` never fires on touch,
//    so on a phone the glyph alone conveys nothing. A unit test can assert the
//    string exists in the source; only a browser can assert a reader can get
//    at it.
//  * **The 320px case.** The warning used to be clipped away entirely by a
//    `truncate` on the row BUTTON (see the comment on the member row in
//    members.rs, and the repro `"Ian Clarke" + "\u{3000}".repeat(13) + "."`).
//    That class of bug is invisible to every assertion that is not a real
//    layout at a real width.

const WARNING = "\u{26a0}";
const MODAL = '[data-testid="member-info-modal"]';
const MODAL_WARNING_TAG = '[data-testid="member-info-impersonation-tag"]';
const MODAL_WARNING_TEXT = '[data-testid="member-info-impersonation-explanation"]';
const MODAL_IMITATED_NAME = '[data-testid="member-info-impersonated-name"]';
const MODAL_DEPUTY_TAG = '[data-testid="member-info-deputy-tag"]';
const CONVERSATION_WARNING = '[data-testid="message-author-impersonation-warning"]';

// Every member-list badge carries its own `data-testid` (`MemberTag.test_id`),
// so these address the badge directly rather than filtering rows by glyph. A
// glyph filter would also match a nickname that merely CONTAINS the character,
// which is the kind of thing this feature exists to be careful about.
const MEMBER_ROW = 'button[title^="Member ID"]';
const LIST_WARNING = '[data-testid="member-list-impersonation-warning"]';
const LIST_DEPUTY = '[data-testid="member-list-deputy"]';
const rowsWithWarning = (page: Page) =>
  page.locator(MEMBER_ROW).filter({ has: page.locator(LIST_WARNING) });
const rowsWithShield = (page: Page) =>
  page.locator(MEMBER_ROW).filter({ has: page.locator(LIST_DEPUTY) });

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
}

async function openTeamChat(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await page.getByText("Team Chat Room").first().click();
  await page.locator(MEMBER_ROW).first().waitFor({ state: "visible", timeout: 15_000 });
  // Wait for the badge itself, not merely for the list to have rows.
  //
  // The member list paints before the impersonation sweep has produced a
  // result, so a count assertion made on "first row visible" can run against a
  // half-populated list and see zero warnings. That is a real ordering in the
  // app, not a slow machine, so the fix is to synchronise on the thing under
  // test rather than to raise a timeout.
  await page.locator(LIST_WARNING).first().waitFor({ state: "visible", timeout: 15_000 });
}

test.describe("Impersonation warning (#489)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the impostor's row carries ⚠ and the deputy's does not", async ({ page }) => {
    await openTeamChat(page);

    // Exactly one member is flagged. More than one would mean the warning is
    // firing on someone the fixture did not make an impostor — the
    // false-positive failure the tier-1-only decision exists to prevent.
    await expect(rowsWithWarning(page)).toHaveCount(1);

    // And it is NOT the deputy — in THIS fixture.
    //
    // Read the scope carefully, because the general rule is the opposite of
    // what this line looks like. 🛡 and ⚠ are NOT mutually exclusive: with the
    // appointer gate gone, a sockpuppet deputised under a real moderator's name
    // injects that name into the protected set, and then the genuine moderator
    // is flagged too (with the tooltip reworded for a member who does hold
    // privilege). Suppressing the badge for badged members was considered and
    // deliberately rejected, because it would hand exactly that sockpuppet
    // immunity.
    //
    // What holds here is narrower and is a property of the fixture: there is
    // ONE deputy, the impostor is a plain member contributing no protected
    // name, and a member is never accused of impersonating their own identity
    // (the exemption is keyed on the unforgeable `MemberId`). So a ⚠ on this
    // shielded row would mean that per-name exemption broke — which would be
    // worse than shipping nothing. Do not generalise this assertion to "a
    // deputy is never warned".
    await expect(rowsWithShield(page)).toHaveCount(1);
    await expect(
      page
        .locator(MEMBER_ROW)
        .filter({ has: page.locator(LIST_DEPUTY) })
        .filter({ has: page.locator(LIST_WARNING) }),
    ).toHaveCount(0);
  });

  test("the impostor's message author line carries ⚠", async ({ page }) => {
    await openTeamChat(page);

    const badge = page.locator(CONVERSATION_WARNING).first();
    await expect(badge).toBeVisible({ timeout: 15_000 });
    // The tooltip must name the remedy, not merely alarm — and must never
    // contain a nickname (see `tooltip_contains_no_nickname_content`).
    await expect(badge).toHaveAttribute("title", /is NOT a moderator/i);
  });

  test("the impostor's modal EXPLAINS the warning in visible text", async ({ page }) => {
    await openTeamChat(page);

    // Read the DEPUTY's name first, so the imitated-name chip can be compared
    // to it rather than merely checked for being non-empty. Rendering the
    // FLAGGED member's own name there would pass a non-empty check, and under
    // the two bar-class branches of `confusable_variant` the two names differ
    // by a single character — so it would also survive inspection by eye.
    // Read the NAME span rather than stripping glyphs out of the whole row:
    // a row's text is "<nickname> <badges>", and the badge set is fixture-
    // dependent (🌐 / 🔑 / 🎪 / 🔭 all render there under other conditions).
    // Stripping a fixed glyph list would silently poison this string the first
    // time the fixture grew another badge.
    const deputyName = (
      (await rowsWithShield(page)
        .first()
        .locator("span:not(.member-icon)")
        .first()
        .textContent()) || ""
    ).trim();
    expect(deputyName, "could not read the deputy's name").not.toBe("");

    await rowsWithWarning(page).first().click();
    await expect(page.locator(MODAL)).toBeVisible({ timeout: 5_000 });

    await expect(page.locator(MODAL_WARNING_TAG)).toBeVisible();

    // The point of the whole surface: the sentence is READABLE, with no hover.
    const explanation = page.locator(MODAL_WARNING_TEXT);
    await expect(explanation).toBeVisible();
    await expect(explanation).toContainText(/is NOT a moderator/i);

    // The imitated name is shown as its own element, never folded into the
    // sentence above it — and it is the DEPUTY's name, not the flagged
    // member's own.
    await expect(page.locator(MODAL_IMITATED_NAME)).toBeVisible();
    await expect(page.locator(MODAL_IMITATED_NAME)).toHaveText(deputyName);
  });

  // Again: fixture-scoped, not a general rule. See the note in the row test —
  // a genuine deputy CAN carry both badges once someone deputises a sockpuppet
  // under their name. This fixture has one deputy and a plain-member impostor,
  // so the only ⚠ belongs to the impostor.
  test("the deputy's modal shows 🛡 and no ⚠", async ({ page }) => {
    await openTeamChat(page);
    await rowsWithShield(page).first().click();
    await expect(page.locator(MODAL)).toBeVisible({ timeout: 5_000 });

    await expect(page.locator(MODAL_DEPUTY_TAG)).toBeVisible();
    await expect(page.locator(MODAL_WARNING_TAG)).toHaveCount(0);
    await expect(page.locator(MODAL_WARNING_TEXT)).toHaveCount(0);
  });
});

test.describe("Impersonation warning survives a narrow viewport (#489)", () => {
  // 320px — the narrowest width the responsive layout targets
  // (responsive-layout.spec.ts uses the same). The warning was previously
  // clipped off the row entirely by a `truncate` on the button; only the NAME
  // may truncate, and every badge is `flex-shrink-0`.
  test.use({ viewport: { width: 320, height: 568 } });

  /// Open Team Chat's member list at 320px.
  ///
  /// At 320px no room is selected yet and the room-list panel is hidden, so
  /// the room cannot be clicked at all. `responsive-layout.spec.ts` hit the
  /// same wall and solved it by selecting at desktop width and resizing back;
  /// the LAYOUT under test is still the 320px one, which is what matters here.
  async function openTeamChatAtNarrowWidth(page: Page) {
    await page.goto("/");
    await waitForApp(page);

    const roomBtn = page.getByRole("button", { name: "Team Chat Room" });
    const vp = page.viewportSize();
    await page.setViewportSize({ width: 1280, height: vp?.height ?? 568 });
    await expect(roomBtn).toBeVisible({ timeout: 15_000 });
    await roomBtn.click();
    await expect(page.getByRole("heading", { name: "Team Chat Room" })).toBeVisible({
      timeout: 15_000,
    });
    await page.setViewportSize({ width: 320, height: vp?.height ?? 568 });

    // Now switch to the members panel, which is a separate view at this width.
    const header = page.locator(".border-b.border-border.bg-panel");
    await header.locator("button").last().click();
    await expect(page.locator("aside").filter({ hasText: "Active Members" })).toBeVisible({
      timeout: 15_000,
    });

    const memberList = page.locator('[data-testid="member-list"]');
    await memberList.waitFor({ state: "visible", timeout: 15_000 });
  }

  test("the ⚠ is still visible at 320px", async ({ page }) => {
    await openTeamChatAtNarrowWidth(page);

    // FORCE the row to overflow before measuring anything.
    //
    // Without this the test is vacuous, and quietly so: the example data's
    // nicknames are short generated names, so at 320px the row does not
    // overflow, nothing is clipped, and the assertions below would pass
    // whether or not the clipping bug were present. The bug only bites when
    // the name is longer than the row — the original repro was
    // `"Ian Clarke" + "\u{3000}".repeat(13) + "."`.
    //
    // Squeezing the panel reproduces exactly that condition without putting an
    // absurd nickname into the demo data every developer sees. What is under
    // test is the layout rule, not the specific name: however long the name,
    // only the NAME may truncate and the badges must survive.
    await page.addStyleTag({
      content:
        'aside:has([data-testid="member-list"]) { width: 150px !important;' +
        " max-width: 150px !important; min-width: 0 !important; }",
    });

    const flagged = rowsWithWarning(page).first();
    await expect(flagged).toBeVisible();

    // Measure the row, the name and the badge in ONE evaluate, so every number
    // comes from the same layout pass.
    const m = await flagged.evaluate((el) => {
      const btn = el as HTMLElement;
      const name = btn.querySelector("span:not(.member-icon)") as HTMLElement;
      const icon = btn.querySelector(
        '[data-testid="member-list-impersonation-warning"]',
      ) as HTMLElement | null;
      return {
        buttonRight: btn.getBoundingClientRect().right,
        badgeRight: icon ? icon.getBoundingClientRect().right : null,
        badgeWidth: icon ? icon.getBoundingClientRect().width : 0,
        nameScrollW: name ? name.scrollWidth : 0,
        nameClientW: name ? name.clientWidth : 0,
      };
    });

    // THE PROPERTY, asserted FIRST so a broken layout fails with the message
    // that describes the bug. `toBeVisible` alone is not enough — a badge
    // pushed outside an `overflow:hidden` row still reports a box, which is
    // exactly how the original bug presented.
    expect(m.badgeRight, "the ⚠ is not in the row at all").not.toBeNull();
    expect(m.badgeWidth, "the ⚠ has zero width").toBeGreaterThan(0);
    expect(
      m.badgeRight as number,
      "the ⚠ is clipped past the right edge of its row",
    ).toBeLessThanOrEqual(m.buttonRight + 1);

    // PREMISE, not decoration: the squeeze must actually be clamping the name.
    // If it is not, the row has room for everything and the assertion above
    // would pass with or without the bug.
    //
    // Checked SECOND and deliberately so. `scrollWidth`/`clientWidth` are 0 for
    // an INLINE element, and putting `truncate` back on the row button (the
    // regression this test exists for) stops the button being a flex container,
    // which makes both spans inline and both numbers 0. Run first, this would
    // have failed the mutation check with "the name is not being clamped",
    // which is true but describes a symptom rather than the clipping.
    expect(
      m.nameScrollW,
      "the name is not being clamped (or the row stopped being a flex " +
        "container, which makes these numbers 0), so this test cannot " +
        "detect clipping",
    ).toBeGreaterThan(m.nameClientW);
  });

  // The reason this whole PR exists: `title=` never fires on touch, so the
  // modal is the only place a phone user can READ the warning. A test that
  // only ever opens it at 1280px would not have noticed if it were unreadable
  // on the device the feature is for.
  test("the modal's explanation is readable at 320px, and cannot widen the page", async ({
    page,
  }) => {
    await openTeamChatAtNarrowWidth(page);

    await rowsWithWarning(page).first().click();
    await expect(page.locator(MODAL)).toBeVisible({ timeout: 15_000 });

    const explanation = page.locator(MODAL_WARNING_TEXT);
    await expect(explanation).toBeVisible();
    await expect(explanation).toContainText(/is NOT a moderator/i);
    await expect(page.locator(MODAL_IMITATED_NAME)).toBeVisible();

    // The nickname in that chip is ATTACKER-CHOSEN, so it can be long and
    // contain no break opportunity at all — which is the case
    // `member_info_modal.rs`'s `max-w-full break-words` exists for. The
    // fixture's generated names are short, so as with the row test above this
    // has to force the condition or it measures nothing.
    const forced = await page.locator(MODAL_IMITATED_NAME).evaluate((el) => {
      const span = el as HTMLElement;
      span.textContent = "W".repeat(160);

      // PREMISE: the injected text must actually be wider than the panel that
      // has to contain it. Measured on an off-screen clone with wrapping
      // disabled, so this is the text's NATURAL width, not the wrapped one.
      const probe = span.cloneNode(true) as HTMLElement;
      probe.style.position = "absolute";
      probe.style.left = "-9999px";
      probe.style.whiteSpace = "nowrap";
      probe.style.width = "max-content";
      probe.style.maxWidth = "none";
      document.body.appendChild(probe);
      const naturalWidth = probe.getBoundingClientRect().width;
      probe.remove();

      const panel = span.closest('[data-testid="member-info-modal"]') as HTMLElement;
      return { naturalWidth, panelWidth: panel.getBoundingClientRect().width };
    });
    expect(
      forced.naturalWidth,
      "the forced nickname is not wider than the modal panel, so nothing " +
        "here could overflow and this test proves nothing",
    ).toBeGreaterThan(forced.panelWidth);

    // THE PROPERTY: a nickname nobody can break must not make the whole page
    // scroll sideways. `scrollWidth` is read off the document element, so it
    // catches the panel pushing the layout wide even when the chip itself
    // reports a box that fits.
    const overflow = await page.evaluate(() => ({
      pageScrollWidth: document.documentElement.scrollWidth,
      viewport: window.innerWidth,
    }));
    expect(
      overflow.pageScrollWidth,
      "an unbreakable nickname in the impersonation chip pushed the page " +
        "wider than the viewport — the modal's `max-w-full break-words` is " +
        "what prevents that",
    ).toBeLessThanOrEqual(overflow.viewport + 1);
  });
});
