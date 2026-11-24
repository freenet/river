import { test, expect, Page } from "@playwright/test";
import { waitForApp, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";

// freenet/river#1023: the admission proof of work runs in Web Workers, off the
// page's main thread. The fixture rooms (see public-room-join.spec.ts) are
// `open` (4 bits) and `hard` (26 bits, never solved by a test).

async function hooksReady(page: Page) {
  await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
}

async function openRoom(page: Page, kind: "open" | "hard") {
  await page.goto("/");
  await waitForApp(page);
  await hooksReady(page);
  const key = await callRiverTest(page, "viewableRoomKey", kind);
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();
}

test("a join solves in Web Workers and posts exactly once", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await page.getByTestId("viewed-room-composer").fill("From a worker");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "powSolverMode")).toBe("workers");
  expect(await callRiverTest(page, "roomMessageCount", "From a worker")).toBe(1);
  // The solve is over: its workers are gone.
  await expect.poll(() => page.workers().length).toBe(0);
  expect(panics).toEqual([]);
});

test("cancelling a running solve stops its workers and keeps the draft", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await page.getByTestId("viewed-room-composer").fill("Keep me");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
  // Really hashing (26 bits is out of reach of a test), in workers, not on the page.
  await expect.poll(() => callRiverTest(page, "powSolverMode"), { timeout: 30_000 }).toBe("workers");
  expect(page.workers().length).toBeGreaterThan(0);

  await page.getByTestId("viewed-room-join-cancel").click();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Keep me");
  await expect.poll(() => page.workers().length, { timeout: 10_000 }).toBe(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "Keep me")).toBe(0);
  expect(panics).toEqual([]);
});

test("a changed room policy retargets the workers and the message still posts once", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("After retarget");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
  const before = await callRiverTest(page, "powRetargets");
  // More bits and a new epoch: the search restarts for the new policy.
  expect(await callRiverTest(page, "retargetPolicy", "open", 10, 2)).toBe(true);
  await expect.poll(() => callRiverTest(page, "powRetargets"), { timeout: 10_000 }).toBe(before + 1);
  await callRiverTest(page, "holdSolver", false);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "powSolverMode")).toBe("workers");
  expect(await callRiverTest(page, "roomMessageCount", "After retarget")).toBe(1);
  expect(panics).toEqual([]);
});

test("without Web Workers the join solves on the main thread and still posts once", async ({ page }) => {
  await page.addInitScript(() => {
    (window as any).Worker = undefined;
  });
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await page.getByTestId("viewed-room-composer").fill("Main thread");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "powSolverMode")).toBe("main");
  expect(await callRiverTest(page, "roomMessageCount", "Main thread")).toBe(1);
  expect(panics).toEqual([]);
});

// The Freenet gateway embeds the app in `sandbox="allow-scripts allow-forms
// allow-popups"`: an opaque origin. Workers made from a blob there, loading
// the app's module from the gateway, need the gateway's CORS headers (the app's
// own wasm fetch needs them too). The static server in this suite sends none,
// so the parent adds them to what the iframe loads.
test("a join solves in Web Workers inside the gateway's sandboxed iframe", async ({ page }) => {
  const base = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:8082";
  await page.route(`${base}/**`, async (route) => {
    const response = await route.fetch();
    await route.fulfill({ response, headers: { ...response.headers(), "access-control-allow-origin": "*" } });
  });
  await page.goto(`${base}/`);
  await waitForApp(page);
  await hooksReady(page);
  const key = await callRiverTest(page, "viewableRoomKey", "open");

  page.on("console", (m) => console.log("console:", m.type(), m.text()));
  page.on("pageerror", (e) => console.log("pageerror:", e.message));
  await page.setContent(
    `<iframe id="app" src="${base}/?room=${key}" sandbox="allow-scripts allow-forms allow-popups" style="width:1000px;height:700px"></iframe>`,
  );
  const frame = page.frameLocator("#app");
  await expect(frame.getByTestId("viewed-room-name")).toBeVisible({ timeout: 30_000 });
  await frame.getByTestId("viewed-room-composer").fill("Sandboxed");
  await frame.getByTestId("viewed-room-send").click();
  await expect(frame.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  const handle = await page.locator("#app").elementHandle();
  const inner = (await handle!.contentFrame())!;
  const mode = await inner.evaluate(() => (window as any).__riverTest.powSolverMode());
  expect(mode).toBe("workers");
});
