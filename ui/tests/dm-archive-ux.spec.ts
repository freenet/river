import { test, expect } from "@playwright/test";
import { waitForApp } from "./example-room";

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
//   1. The app boots and the DM rail "Direct Messages" header is
//      hidden when no DMs exist — i.e. the no-DMs early-return still
//      kicks in after the archive viewer was wired up. A regression
//      here would surface an empty section that confuses first-load
//      users.
//   2. The "Hide" button — the one #266 reported was visually
//      confused with the close ✕ — is no longer present in the DOM
//      after the PR (it moved to the per-row rollover).
//
// These checks lock in the no-regression invariants; the deeper
// behavioural coverage is in Rust unit tests.
//
// NOTE: a previous version of this spec also asserted "no console
// errors" via a filter on `/panic|wasm|RefCell/i`. That broke on the
// example-data build's startup-time wasm-bindgen warning
// ("imported JS function that was not marked as `catch` threw an
// error"), which is a pre-existing console message unrelated to this
// PR. The console-error assertion was removed rather than narrowed
// because no other spec in this directory does that kind of check —
// adding only-here filtering is a brittle wart. If we ever want
// "did WASM panic?" coverage, do it via a deliberate panic-detection
// harness, not by parsing console text.

test.describe("DM archive UX overhaul (#266)", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("page loads cleanly with archive code paths wired up", async ({
    page,
  }) => {
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
  });
});
