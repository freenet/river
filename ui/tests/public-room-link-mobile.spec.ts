import { test, expect, Page, FrameLocator, Locator } from "@playwright/test";
import { CLOSE_VIEWER_LABEL, closeViewer, waitForApp, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";

// The room opened from a link on a phone: the rail and the members list are
// separate panels there, so leaving, reading the members and writing each need
// their own control. Runs on the mobile projects only.

async function openOpenRoom(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
  const key = await callRiverTest(page, "viewableRoomKey", "open");
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();
}

test.beforeEach(async ({ page }) => {
  test.skip((page.viewportSize()?.width ?? 1000) >= 768, "phone layout only");
});

test("a visitor on a phone types, answers the name prompt and lands in the room", async ({ page }) => {
  const panics = panicsOf(page);
  await openOpenRoom(page);
  await expect(page.getByTestId("viewed-room-composer")).toBeEnabled();
  await page.getByTestId("viewed-room-composer").fill("Hello from a phone");
  await page.getByTestId("viewed-room-send").click();
  const input = page.getByTestId("viewed-room-nickname-input");
  await expect(input).toBeVisible();
  const box = await page.getByTestId("viewed-room-nickname-continue").boundingBox();
  expect(box!.height).toBeGreaterThanOrEqual(44);
  await input.fill("Phone Pat");
  await page.getByTestId("viewed-room-nickname-continue").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  await expect(page.getByTestId("message-composer").first()).toBeVisible();
  expect(await callRiverTest(page, "roomMessageCount", "Hello from a phone")).toBe(1);
  expect(await callRiverTest(page, "currentRoomSelfNickname")).toBe("Phone Pat");
  expect(panics).toEqual([]);
});

test("on a phone the header x leaves, the members button shows the members, the menu shows the viewed row", async ({ page }) => {
  const panics = panicsOf(page);
  await openOpenRoom(page);
  await expect(page.getByTestId("viewed-room-close-narrow")).toBeVisible();
  await expect(page.getByTestId("viewed-room-close-narrow")).toHaveAttribute("aria-label", CLOSE_VIEWER_LABEL);

  // Members: a panel of its own on a phone, with a way back.
  await page.getByTestId("viewed-room-members-button").click();
  await expect(page.getByTestId("members-rail")).toBeVisible();
  expect(await page.getByTestId("viewed-room-member").count()).toBeGreaterThan(0);
  await page.getByTestId("members-back-to-chat-button").click();
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();

  // The rooms panel shows the viewed room's row, and tapping it returns.
  await page.getByRole("button", { name: "Open room list" }).first().click();
  await expect(page.getByTestId("viewed-room-rail-row")).toBeVisible();
  await page.getByTestId("viewed-room-rail-name").click();
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();

  await closeViewer(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  expect(panics).toEqual([]);
});

// Short viewports: a phone with the soft keyboard up leaves 200-400 px. Every
// new control must be reachable by a tap, in the page and in the gateway's
// sandboxed iframe (which is as tall as the phone's viewport).
const SHORT_VIEWPORTS = [
  { name: "Pixel 7 with keyboard", width: 412, height: 400 },
  { name: "iPhone SE with keyboard", width: 320, height: 260 },
  { name: "phone in landscape with keyboard", width: 667, height: 200 },
];

async function expectTappable(page: Page, el: Locator) {
  await el.scrollIntoViewIfNeeded();
  const box = (await el.boundingBox())!;
  const vp = page.viewportSize()!;
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.y).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(vp.width);
  expect(box.y + box.height).toBeLessThanOrEqual(vp.height);
}

for (const vp of SHORT_VIEWPORTS) {
  test(`on a short phone viewport (${vp.name}) the name prompt's Continue, the header x, the composer and Cancel can be tapped`, async ({ page }) => {
    const panics = panicsOf(page);
    await page.setViewportSize({ width: vp.width, height: vp.height });
    await openOpenRoom(page);
    await expectTappable(page, page.getByTestId("viewed-room-close-narrow"));
    await expectTappable(page, page.getByTestId("viewed-room-composer"));
    await callRiverTest(page, "holdSolver", true);
    await page.getByTestId("viewed-room-composer").fill("Short screen");
    await page.getByTestId("viewed-room-send").click();
    await expect(page.getByTestId("viewed-room-nickname-input")).toBeVisible();
    const cont = page.getByTestId("viewed-room-nickname-continue");
    await expectTappable(page, cont);
    await cont.click();
    await expect(page.getByTestId("viewed-room-nickname-modal")).toHaveCount(0);
    const cancel = page.getByTestId("viewed-room-join-cancel");
    await expectTappable(page, cancel);
    await cancel.click();
    await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
    await callRiverTest(page, "holdSolver", false);
    await closeViewer(page);
    expect(panics).toEqual([]);
  });

  test(`in the sandboxed iframe on a short phone viewport (${vp.name}) Continue is tapped, not keyed`, async ({ page }) => {
    const base = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:8082";
    await page.setViewportSize({ width: vp.width, height: vp.height });
    // The gateway's sandbox policy, as pow-worker.spec.ts serves it.
    await page.route(`${base}/**`, async (route) => {
      const response = await route.fetch();
      const headers: Record<string, string> = { ...response.headers(), "access-control-allow-origin": "*" };
      if (route.request().resourceType() === "document") {
        headers["content-security-policy"] =
          `sandbox allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-downloads allow-modals; ` +
          `default-src ${new URL(base).origin} 'unsafe-inline' 'unsafe-eval' blob: data:; connect-src ${new URL(base).origin} blob: data:`;
      }
      await route.fulfill({ response, headers });
    });
    await page.goto(`${base}/`);
    await waitForApp(page);
    await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
    const key = await callRiverTest(page, "viewableRoomKey", "open");
    await page.setContent(
      `<body style="margin:0"><iframe id="app" src="${base}/?room=${key}" sandbox="allow-scripts allow-forms allow-popups" style="width:100vw;height:100vh;border:0"></iframe></body>`,
    );
    const frame: FrameLocator = page.frameLocator("#app");
    await expect(frame.getByTestId("viewed-room-name")).toBeVisible({ timeout: 30_000 });
    await frame.getByTestId("viewed-room-composer").fill("Framed");
    await frame.getByTestId("viewed-room-send").click();
    const cont = frame.getByTestId("viewed-room-nickname-continue");
    await expect(cont).toBeVisible();
    await expectTappable(page, cont);
    await cont.click();
    await expect(frame.getByTestId("viewed-room-nickname-modal")).toHaveCount(0);
  });
}
