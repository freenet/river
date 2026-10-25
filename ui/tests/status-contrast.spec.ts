import { test, expect, Locator, Page } from "@playwright/test";
import { colourOf, contrast, effectiveBackground, over, resolveColor, selectRoom, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";
import { DM_PEER } from "./dm-thread";

// Warning and destructive surfaces, in both themes: their text against what
// is painted behind it (alpha and opacity composited, as shell-contrast
// does), and white on every destructive fill. Raw palette shades used to
// paint these (light yellow on light yellow in the leave confirmation,
// white on red-500 at 3.8:1); they now go through tokens checked here.

async function expectReadable(el: Locator, what: string, min = 4.5) {
  await expect(el, `${what} is shown`).toBeVisible();
  const opacity = await el.evaluate(async (e) => {
    await Promise.all(e.getAnimations({ subtree: false }).map((a) => a.finished.catch(() => undefined)));
    let o = 1;
    for (let n: Element | null = e; n; n = n.parentElement) o *= parseFloat(getComputedStyle(n).opacity);
    return o;
  });
  const fg = await colourOf(el);
  const bg = await effectiveBackground(el);
  const ratio = contrast(over([fg[0], fg[1], fg[2], (fg[3] ?? 1) * opacity], bg), bg);
  expect(ratio, `${what}: ${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(min);
}

async function open(page: Page, room: string) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, room);
  await page.mouse.move(0, 0);
}

async function openMemberInfo(page: Page, which: Locator) {
  await which.click();
  await expect(page.getByTestId("member-info-modal")).toBeVisible();
}

const rows = (page: Page) => page.getByTestId("member-list").locator('[data-testid^="member-item-"] button');

/// The status tokens against the surfaces they are drawn on, resolved in
/// the page so each theme's overrides are what is measured: text tokens on
/// the panel, the raised surface and their own callout fill, and body text
/// on the callout fills. Composited, since a token may be translucent.
const TOKEN_PAIRS: [string, string][] = [
  ...["--color-danger-text", "--color-warning-text", "--color-success-text"].flatMap((fg): [string, string][] => [
    [fg, "--color-panel"],
    [fg, "--color-surface"],
  ]),
  ["--color-danger-text", "--color-error-bg"],
  ["--color-warning-text", "--color-warning-bg"],
  ["--color-success-text", "--color-success-bg"],
  ["--color-text", "--color-warning-bg"],
  ["--color-text", "--color-error-bg"],
];

for (const scheme of ["light", "dark"] as const) {
  test(`${scheme}: status tokens clear 4.5:1 on the surfaces they sit on`, { tag: "@chromium-only" }, async ({ page }) => {
    await page.emulateMedia({ colorScheme: scheme });
    await page.goto("/");
    await waitForApp(page);
    const panel = await resolveColor(page, "var(--color-panel)");
    for (const [fg, bg] of TOKEN_PAIRS) {
      // The background over the panel (a callout fill may be translucent),
      // then the text over that.
      const behind = over(await resolveColor(page, `var(${bg})`), panel);
      const ratio = contrast(over(await resolveColor(page, `var(${fg})`), behind), behind);
      expect(ratio, `${fg} on ${bg}: ${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(4.5);
    }
    // White on the destructive fill and its hover.
    for (const fill of ["--color-danger", "--color-danger-hover"]) {
      const behind = over(await resolveColor(page, `var(${fill})`), panel);
      const ratio = contrast([255, 255, 255, 1], behind);
      expect(ratio, `white on ${fill}: ${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(4.5);
    }
  });
}

for (const scheme of ["light", "dark"] as const) {
  test.describe(`Warning and destructive surfaces, ${scheme}`, { tag: "@chromium-only" }, () => {
    test.use({ colorScheme: scheme, viewport: { width: 1280, height: 800 } });

    test("leave-room confirmation", async ({ page }) => {
      await open(page, "Team Chat Room");
      await page.getByTestId("room-info-button").click();
      await page.getByTestId("edit-room-leave-button").click();
      await expectReadable(page.getByText("Are you sure you want to leave this room?", { exact: false }), "the leave warning");
      await page.mouse.move(0, 0);
      await expectReadable(page.getByRole("button", { name: "Confirm Leave" }), "Confirm Leave on its fill");
    });

    test("export identity warning", async ({ page }) => {
      await open(page, "Your Private Room");
      await page.getByTestId("export-id-button").click();
      await expectReadable(page.getByText("This token contains your private key", { exact: false }), "the export warning");
    });

    test("ban confirmation", async ({ page }) => {
      await open(page, "Your Private Room");
      await openMemberInfo(page, rows(page).filter({ hasNot: page.getByTestId("member-list-self") }).first());
      await page.getByRole("button", { name: "Ban User" }).click();
      await page.mouse.move(0, 0);
      await expectReadable(page.getByText("Are you sure you want to ban", { exact: false }), "the ban question");
      await expectReadable(page.getByRole("button", { name: "Yes, Ban User" }), "Yes, Ban User on its fill");
    });

    test("delete-message confirmation", async ({ page }) => {
      await open(page, "Your Private Room");
      const own = page.locator('[data-testid="message-row"][data-message-self="true"]').last();
      await own.scrollIntoViewIfNeeded();
      await own.hover({ position: { x: 4, y: 4 } });
      await own.getByTestId("message-hover-actions").getByRole("button", { name: "Delete message" }).click();
      await page.mouse.move(0, 0);
      await expectReadable(page.getByText("This action cannot be undone.", { exact: false }), "the delete note");
      await expectReadable(page.getByRole("button", { name: "Delete", exact: true }), "Delete on its fill");
    });

    test("delete-their-messages confirmation in a DM thread", async ({ page }) => {
      await open(page, "Team Chat Room");
      await callRiverTest(page, "appendDmsForPeer", DM_PEER.index, 2);
      await page.locator(".dm-rail-row-btn", { hasText: DM_PEER.name }).click();
      await page.getByRole("button", { name: /delete their messages/i }).click();
      const dialog = page.getByRole("dialog", { name: /confirm delete their messages/i });
      await expect(dialog).toBeVisible();
      await page.mouse.move(0, 0);
      const confirm = dialog.getByRole("button", { name: /delete/i }).last();
      await expectReadable(confirm, "the destructive confirm on its fill");
    });

    test("member info's impersonation explanation", async ({ page }) => {
      await open(page, "Team Chat Room");
      const impostor = rows(page).filter({ has: page.getByTestId("member-list-impersonation-warning") }).first();
      await openMemberInfo(page, impostor);
      const explanation = page.getByTestId("member-info-impersonation-explanation");
      await expectReadable(explanation.locator("p").first(), "the impersonation explanation");
    });

    test("not-a-member notice", async ({ page }) => {
      await open(page, "Public Discussion Room");
      const notice = page.getByText("You're not a member of this room yet", { exact: false });
      await expectReadable(notice, "the not-a-member notice");
    });
  });
}
