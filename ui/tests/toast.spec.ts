import { test, expect, Locator, Page } from "@playwright/test";
import { waitForApp, nextFrames } from "./example-room";
import { callRiverTest } from "./river-test";

// The app-wide info toast (`components/toast.rs`): one at a time, its action
// and Dismiss each close it, it paints above the ordinary modals, wraps at
// 320px, and stays clear of the generation-update banner.
//
// The id guard (a replaced toast's timer or click cannot close its
// replacement) and the overlap rule are unit tests in toast.rs; expiry in real time is in
// dm-archive-ux.spec.ts, through the archive Undo that uses it.

const LONG =
  "Invitation to \"A room with a really quite remarkably long name that has to wrap\" sent";

function toast(page: Page) {
  return page.getByTestId("toast");
}

async function open(page: Page) {
  await page.goto("/");
  await waitForApp(page);
}

async function show(page: Page, message: string, action: string | null = null) {
  await callRiverTest(page, "showToast", message, action);
  await expect(page.getByTestId("toast-message")).toHaveText(message);
  // The entrance is 150ms; measure where the card ends up.
  await toast(page).evaluate((el) => Promise.all(el.getAnimations().map((a) => a.finished)));
  await nextFrames(page);
}

/// Whether a click at the centre of `el` reaches `el` (or something in it).
async function hitTestable(el: Locator): Promise<boolean> {
  return el.evaluate((node) => {
    const r = node.getBoundingClientRect();
    const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return hit !== null && node.contains(hit);
  });
}

async function withinViewport(page: Page, el: Locator) {
  const box = (await el.boundingBox())!;
  const vp = page.viewportSize()!;
  expect(box.x, "left edge on screen").toBeGreaterThanOrEqual(0);
  expect(box.x + box.width, "right edge on screen").toBeLessThanOrEqual(vp.width);
  expect(box.y, "top edge on screen").toBeGreaterThanOrEqual(0);
  expect(box.y + box.height, "bottom edge on screen").toBeLessThanOrEqual(vp.height);
}

test.describe("App toast", () => {
  test("a new toast replaces the visible one", async ({ page }) => {
    await open(page);
    await show(page, "First");
    await show(page, "Second");
    await expect(page.getByTestId("toast-message")).toHaveText("Second");
  });

  test("the action runs once and closes the toast; Dismiss only closes it", async ({ page }) => {
    await open(page);
    await callRiverTest(page, "takeToastActionRuns");

    await show(page, "With an action", "Do it");
    await page.getByTestId("toast-action").click();
    await expect(toast(page)).toHaveCount(0);
    expect(await callRiverTest(page, "takeToastActionRuns")).toBe(1);

    await show(page, "Dismiss me", "Do it");
    await page.getByRole("button", { name: "Dismiss notification" }).click();
    await expect(toast(page)).toHaveCount(0);
    expect(await callRiverTest(page, "takeToastActionRuns")).toBe(0);
  });

  test("announces through one polite status region that is always there", async ({ page }) => {
    await open(page);
    const host = page.getByTestId("toast-host");
    await expect(host).toHaveAttribute("role", "status");
    await expect(host).toHaveAttribute("aria-live", "polite");
    await expect(host).toBeEmpty();

    await show(page, "Announced");
    await expect(host.getByTestId("toast-message")).toHaveText("Announced");
    // The toast never takes focus.
    expect(await page.evaluate(() => document.activeElement?.closest("[data-testid=toast]") ?? null)).toBeNull();
  });

  test("paints above an open modal", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await open(page);
    await page.getByTestId("create-room-button").click();
    await expect(page.getByRole("heading", { name: "Create New Room", exact: true })).toBeVisible();

    await show(page, "Above the dialog", "Do it");
    for (const el of [page.getByTestId("toast-message"), page.getByTestId("toast-action"), page.getByTestId("toast-dismiss")]) {
      expect(await hitTestable(el), "the modal's backdrop must not cover the toast").toBe(true);
    }
  });

  test("wraps at 320px with its buttons on screen and reachable", async ({ page }) => {
    await page.setViewportSize({ width: 320, height: 568 });
    await open(page);
    await show(page, LONG, "Undo");

    const message = page.getByTestId("toast-message");
    const lineHeight = await message.evaluate((el) => parseFloat(getComputedStyle(el).lineHeight));
    expect((await message.boundingBox())!.height, "the long text wraps").toBeGreaterThan(lineHeight * 1.5);
    for (const el of [toast(page), page.getByTestId("toast-action"), page.getByTestId("toast-dismiss")]) {
      await withinViewport(page, el);
      expect(await hitTestable(el)).toBe(true);
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  });

  test("its action and Dismiss are in the tab order", { tag: "@chromium-only" }, async ({ page }) => {
    await open(page);
    await show(page, "Keyboard", "Do it");
    // The toast is last in the document, so backwards from the end reaches it first.
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.keyboard.press("Shift+Tab");
    await expect(page.getByTestId("toast-dismiss")).toBeFocused();
    await page.keyboard.press("Shift+Tab");
    await expect(page.getByTestId("toast-action")).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(toast(page)).toHaveCount(0);
    expect(await callRiverTest(page, "takeToastActionRuns")).toBe(1);
  });

  test("enters with its own keyframes, and only fades under reduced motion", async ({ page }) => {
    await open(page);
    await show(page, "Moving");
    expect(
      await toast(page).evaluate((el) => {
        const s = getComputedStyle(el);
        return [s.animationName, s.animationDuration];
      }),
    ).toEqual(["app-toast-enter", "0.15s"]);

    await page.emulateMedia({ reducedMotion: "reduce" });
    await show(page, "Still");
    expect(await toast(page).evaluate((el) => getComputedStyle(el).animationName)).toBe("app-toast-fade-in");
  });
});

test.describe("App toast with the update banner", () => {
  test.afterEach(async ({ page }) => {
    await callRiverTest(page, "showUpdateNotice", null).catch(() => {});
  });

  for (const vp of [
    { width: 320, height: 480 },
    { width: 1280, height: 400 },
  ]) {
    test(`both stay readable and usable at ${vp.width}x${vp.height}`, async ({ page }) => {
      await page.setViewportSize(vp);
      await open(page);
      await callRiverTest(page, "showUpdateNotice", "countdown");
      const banner = page.getByTestId("update-banner");
      await expect(banner).toBeVisible();

      await show(page, LONG, "Undo");
      await expect
        .poll(async () => {
          const t = (await toast(page).boundingBox())!;
          const b = (await banner.boundingBox())!;
          return t.y + t.height <= b.y || b.y + b.height <= t.y;
        }, { message: "the toast and the banner must not overlap" })
        .toBe(true);

      for (const el of [
        page.getByTestId("toast-message"),
        page.getByTestId("toast-action"),
        page.getByTestId("toast-dismiss"),
        banner.locator("span").first(),
        page.getByTestId("update-banner-reload"),
        page.getByTestId("update-banner-later"),
      ]) {
        await withinViewport(page, el);
        expect(await hitTestable(el)).toBe(true);
      }
    });
  }

  test("a toast that reaches the banner moves below it, and goes back when the banner goes", async ({ page }) => {
    await page.setViewportSize({ width: 320, height: 568 });
    await open(page);
    await show(page, LONG, "Undo");
    const ownTop = (await toast(page).boundingBox())!.y;

    await callRiverTest(page, "showUpdateNotice", "manual");
    await expect(page.getByTestId("update-banner")).toBeVisible();
    await expect.poll(async () => (await toast(page).boundingBox())!.y).toBeGreaterThan(ownTop);

    await callRiverTest(page, "showUpdateNotice", null);
    await expect(page.getByTestId("update-banner")).toHaveCount(0);
    await expect.poll(async () => (await toast(page).boundingBox())!.y).toBe(ownTop);
  });
});
