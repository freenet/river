import { test, expect } from "@playwright/test";

// End-to-end coverage for freenet/river#584: the viewer's own row is pinned to
// index 0 of the member list, above the room owner.
//
// The Rust side pins the helper (`pin_self_to_top`) and that `MemberList` calls
// it, but neither can prove the row actually lands first in the DOM — the hoist
// runs inside a `use_memo` no unit test can drive, which is exactly why the
// PR's original source-grep pin was the only thing standing between this
// behaviour and nothing. This spec is the real proof.
//
// Non-vacuity, and it is load-bearing here: example data's "Team Chat Room"
// makes the local user a plain member (`SelfIs::Member`, ui/src/example_data.rs)
// and the owner is the root of the invite tree, so WITHOUT the pin index 0 is
// the OWNER's row. Asserting that the owner exists but is not first is what
// makes a deleted pin fail here instead of passing silently — a bare "row 0 is
// self" check would also pass in a fixture where self happened to sort first.

const SELF_TAG = '[data-testid="member-list-self"]';
const OWNER_TAG = '[data-testid="member-list-owner"]';

test.describe("Member list pins the viewer's own row (#584)", () => {
  // Fixed desktop viewport so the member list is always in-panel and no
  // mobile view-switching is needed (mirrors member-info-deputy-tag.spec.ts).
  test.use({ viewport: { width: 1280, height: 800 } });

  test("your own row is first, above the room owner", async ({ page }) => {
    await page.goto("/");
    await page.waitForSelector(".app-root", { timeout: 30_000 });
    await page.getByText("Team Chat Room").first().click();

    const rows = page.locator('[data-testid="member-list"] li');
    await rows.first().waitFor({ state: "visible", timeout: 15_000 });

    // PREMISE, asserted first so a broken fixture fails as a broken fixture:
    // there is more than one row, and the owner is on one of them. In a
    // single-row list every assertion below would hold trivially.
    expect(await rows.count()).toBeGreaterThan(1);
    await expect(page.locator(OWNER_TAG)).toHaveCount(1);

    // THE PROPERTY: row 0 is the viewer's own row.
    await expect(rows.first().locator(SELF_TAG)).toHaveCount(1);

    // ...and it displaced the owner, who is the invite-tree root and is index 0
    // without the pin. This is the assertion that goes red if the pin is
    // removed from `MemberList`.
    await expect(rows.first().locator(OWNER_TAG)).toHaveCount(0);
  });
});
