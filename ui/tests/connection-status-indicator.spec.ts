import { test, expect, Locator, Page } from "@playwright/test";

// Regression tests for Bug #5 (Ivvor, Matrix 2026-05-17): the WebSocket
// connection indicator must remain visible when the user has no rooms —
// that's exactly when a brand-new user accepting their first invite
// needs to know whether their node connection is healthy.
//
// Before the fix the indicator lived inside `MemberList`, which returns
// empty when `CURRENT_ROOM` is None. After the fix it lives in
// `RoomList`'s bottom section AND in an inline (`md:hidden`) copy on
// the Conversation panel's Welcome screen, so both desktop and mobile
// no-room flows surface it.
//
// Both placements share the `data-testid="connection-status-indicator"`
// id, so `:visible` is used to pick the one the user actually sees at
// the current viewport.
//
// Hardening (freenet/river#274): the earlier version of this spec only
// asserted `.first()` of the `:visible` set, which silently tolerated a
// regression where `md:hidden` inverted (or the Tailwind v4
// `@source "../src/**/*.rs"` directive was dropped — see AGENTS.md),
// leaving BOTH copies visible or NEITHER. The tests below now assert the
// EXACT visible count (one, never zero or two) at each viewport, that the
// DOM carries exactly the two expected placements, and that the visible
// pill renders a real connection state (matching dot-colour + label)
// rather than a stuck/blank pill.

const PILL = '[data-testid="connection-status-indicator"]';
const VISIBLE_PILL = `${PILL}:visible`;
const ROOMS_RAIL = '[data-testid="rooms-rail"]';
const MEMBERS_RAIL = '[data-testid="members-rail"]';

// Wait for the app shell AND the always-rendered Rooms rail to mount.
// (#274 item 3) The previous `"aside, .app-root button"` selector matched
// any button anywhere in the app, so it could resolve before the rail —
// which carries the persistent indicator — actually rendered. Anchor on
// the rail's stable testid instead.
async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator(ROOMS_RAIL)).toHaveCount(1);
}

// The component maps each `SynchronizerStatus` to a (dot bg-colour, label)
// pair (see `ConnectionStatusIndicator` in ui/src/components/members.rs).
// A genuinely-rendered pill ALWAYS matches one of these rows; a stuck or
// blank pill (e.g. a regression that replaces the reactive `try_read()`
// with a non-subscribing `peek()` and renders no real state) would not.
const STATUS_STATES = [
  { dot: "bg-green-500", label: "Connected" },
  { dot: "bg-yellow-500", label: "Connecting..." },
  { dot: "bg-red-500", label: "Disconnected" },
  // SynchronizerStatus::Error renders "Error: <msg>" with the red dot; the
  // label is matched with a prefix below rather than an exact string.
  { dot: "bg-red-500", label: "Error:" },
];

// Assert the visible pill renders a coherent connection state: its dot
// carries exactly one of the known status colours AND the label text is
// the one paired with that colour. This is the #274 item-2 "detect a
// stuck-state regression" check — a pill that never picks up a real state
// (no subscription) cannot satisfy both halves.
async function expectCoherentState(visiblePill: Locator) {
  const dot = visiblePill.locator("div").first();
  await expect(dot).toBeVisible({ timeout: 5_000 });

  const dotClass = (await dot.getAttribute("class")) ?? "";
  const labelText = ((await visiblePill.textContent()) ?? "").trim();

  // Distinct status colours — `bg-red-500` is shared by Disconnected and
  // Error, so dedupe before counting how many the dot carries.
  const colours = [...new Set(STATUS_STATES.map((s) => s.dot))];
  const present = colours.filter((c) =>
    new RegExp(`(^|\\s)${c}(\\s|$)`).test(dotClass)
  );
  // Exactly one status colour — not zero (blank/stuck dot) and not several
  // (ambiguous render).
  expect(
    present,
    `dot class "${dotClass}" must carry exactly one status colour`
  ).toHaveLength(1);

  const colour = present[0];
  const expectedLabels = STATUS_STATES.filter((s) => s.dot === colour).map(
    (s) => s.label
  );
  const labelMatches = expectedLabels.some((l) =>
    l.endsWith(":") ? labelText.startsWith(l) : labelText === l
  );
  expect(
    labelMatches,
    `label "${labelText}" must match dot colour "${colour}" (one of ${JSON.stringify(
      expectedLabels
    )})`
  ).toBeTruthy();
}

test.describe("Connection status indicator on desktop (Bug #5)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("exactly one indicator is visible on initial load", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    // (#274 item 1) Exactly ONE pill is visible at desktop width — the
    // left-rail copy. The Welcome-screen inline copy is `md:hidden`, so a
    // count of 2 here would mean `md:hidden` inverted; a count of 0 would
    // mean the rail copy got hidden. `.first()` masked both.
    await expect(page.locator(VISIBLE_PILL)).toHaveCount(1);

    // Both placements still exist in the DOM (one shown, one hidden); this
    // guards against the rail copy being dropped entirely — which would
    // also yield a visible count of 1 from the Welcome copy alone if it
    // weren't `md:hidden`.
    await expect(page.locator(PILL)).toHaveCount(2);

    const visiblePill = page.locator(VISIBLE_PILL);
    await expect(visiblePill).toHaveAttribute(
      "aria-label",
      "WebSocket connection status"
    );
  });

  test("the visible indicator lives in the left rail, not the member panel", async ({
    page,
  }) => {
    // Bug #5 root cause was that the indicator only rendered as part of
    // MemberList. We need to assert the visible indicator at desktop
    // width sits inside the always-rendered Rooms rail so the no-room
    // state can't hide it.
    await page.goto("/");
    await waitForApp(page);

    await expect(page.locator(VISIBLE_PILL)).toHaveCount(1);

    const roomsRail = page.locator(ROOMS_RAIL);
    const membersRail = page.locator(MEMBERS_RAIL);

    // The single visible pill at desktop width must be inside the Rooms
    // rail, and it must be the rail's only pill.
    await expect(roomsRail.locator(VISIBLE_PILL)).toHaveCount(1);
    await expect(roomsRail.locator(PILL)).toHaveCount(1);
    // The Members rail must NOT carry an indicator at all (pre-fix
    // location).
    await expect(membersRail.locator(PILL)).toHaveCount(0);
  });

  test("the visible indicator renders a real connection state", async ({
    page,
  }) => {
    // (#274 item 2) Assert the visible pill's dot colour and label form a
    // coherent SynchronizerStatus. A regression that leaves the pill stuck
    // / blank (e.g. swapping the reactive `try_read()` for `peek()`) would
    // fail to render a matching colour+label pair.
    await page.goto("/");
    await waitForApp(page);

    const visiblePill = page.locator(VISIBLE_PILL);
    await expect(visiblePill).toHaveCount(1);
    await expectCoherentState(visiblePill);
  });

  test("indicator remains visible when no room is selected (Welcome screen)", async ({
    page,
  }) => {
    // Initial load has `CURRENT_ROOM = None` — the conversation panel
    // renders "Welcome to River". The indicator must still be visible.
    await page.goto("/");
    await waitForApp(page);

    await expect(page.getByText("Welcome to River")).toBeVisible({
      timeout: 5_000,
    });

    await expect(page.locator(VISIBLE_PILL)).toHaveCount(1);
  });
});

test.describe("Connection status indicator on mobile (Bug #5)", () => {
  // Mobile-Chat is the default `MOBILE_VIEW`, so a brand-new user with
  // no rooms lands on the Welcome screen with the left rail hidden
  // behind `hidden md:flex`. Without the inline Welcome-screen copy of
  // the indicator, Bug #5 would reappear on mobile.
  test.use({ viewport: { width: 375, height: 812 } });

  test("exactly one indicator is visible on the mobile Welcome screen", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await expect(page.getByText("Welcome to River")).toBeVisible({
      timeout: 5_000,
    });

    // (#274 item 1) Exactly ONE pill is visible at mobile width. The
    // left-rail copy lives in the `hidden md:flex` rail wrapper (hidden
    // here), so the single visible pill must be the inline Welcome copy.
    // A count of 2 would mean the rail wrapper failed to hide; a count of
    // 0 would mean the inline `md:hidden` copy is hidden at mobile width
    // too (the original Bug #5 on mobile).
    await expect(page.locator(VISIBLE_PILL)).toHaveCount(1);
    // Both placements still exist in the DOM.
    await expect(page.locator(PILL)).toHaveCount(2);

    // Confirm we're looking at the inline (not left-rail) copy: it sits
    // inside the Welcome-screen heading's container, NOT the Rooms rail.
    const inWelcomeScreen = page
      .locator("h1", { hasText: "Welcome to River" })
      .locator("..")
      .locator(VISIBLE_PILL);
    await expect(inWelcomeScreen).toHaveCount(1);
    await expect(page.locator(ROOMS_RAIL).locator(VISIBLE_PILL)).toHaveCount(0);
  });

  test("the mobile indicator renders a real connection state", async ({
    page,
  }) => {
    // (#274 item 2) Same coherent-state check as desktop, against the
    // inline Welcome-screen copy that mobile users actually see.
    await page.goto("/");
    await waitForApp(page);

    await expect(page.getByText("Welcome to River")).toBeVisible({
      timeout: 5_000,
    });

    const visiblePill = page.locator(VISIBLE_PILL);
    await expect(visiblePill).toHaveCount(1);
    await expectCoherentState(visiblePill);
  });
});
