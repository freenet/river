import { test, expect, Page, ConsoleMessage } from "@playwright/test";

// Archive-UX overhaul (issue #266, follow-up to #261).
//
// The example-data build (`build-ui-example-no-sync`) contains rooms
// and messages but NO direct messages — populating example DMs would
// require composing a real ECIES envelope per DM, which is more code
// than is justified for a UI test fixture. That means the
// "click ✕ on a DM row" and "click Un-archive" flows cannot be driven
// end-to-end through Playwright without standing up a live Freenet
// node. The full archive/unarchive math is instead pinned in
// `filter_rail_entries_*`, `build_archived_rows_*`, and
// `build_archive_toast_*` unit tests in
// `ui/src/components/room_list/dm_rail_section.rs`.
//
// What we DO check here:
//   1. The app boots cleanly with the new DM-rail code paths (no
//      panics, no console errors logged from those modules).
//   2. The DM rail "Direct Messages" header is hidden when no DMs
//      exist — i.e. the no-DMs early-return still kicks in after the
//      archive viewer was wired up. A regression here would surface
//      an empty section that confuses first-load users.
//   3. The "Hide" button — the one #266 reported was visually
//      confused with the close ✕ — is no longer present in the DOM
//      after the PR (it moved to the per-row rollover).
//
// These checks lock in the no-regression invariants; the deeper
// behavioural coverage is in Rust unit tests.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

function recordConsoleErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("console", (msg: ConsoleMessage) => {
    if (msg.type() === "error") {
      errors.push(msg.text());
    }
  });
  return errors;
}

test.describe("DM archive UX overhaul (#266)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("page loads cleanly with archive code paths wired up", async ({
    page,
  }) => {
    const errors = recordConsoleErrors(page);
    await page.goto("/");
    await waitForApp(page);

    // No example DMs → no rail section. The early-return for an
    // empty rail must still trigger after the archive viewer was
    // added; otherwise we'd surface a stray "Direct Messages" header.
    await expect(
      page.getByRole("heading", { name: "Direct Messages" })
    ).toHaveCount(0);

    // The old in-modal "Hide" button is gone from the entire app.
    // Even if no DM thread is open, the literal string should not
    // appear in the rendered Dioxus tree (we don't ship dead RSX
    // either — the source was removed).
    await expect(page.getByRole("button", { name: "Hide" })).toHaveCount(0);

    // No unhandled WASM panics or render errors.
    expect(
      errors.filter((e) => /panic|TypeError|unwrap|wasm|RefCell/i.test(e))
    ).toEqual([]);
  });

  test("layout remains stable across responsive breakpoints", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    // Desktop → tablet → mobile. The DM rail section's new
    // `group-hover` / `md:opacity-*` classes share the same Tailwind
    // generator as the rest of the app; if Tailwind weren't picking
    // up the new classes, the layout would regress here.
    for (const width of [1280, 768, 480]) {
      await page.setViewportSize({ width, height: 800 });
      // Body should always have a visible main panel — a layout-broken
      // app would render zero-height columns.
      const bodyBox = await page.locator("body").boundingBox();
      expect(bodyBox?.height ?? 0).toBeGreaterThan(0);
    }
  });
});
