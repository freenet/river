import { test, expect, Page, FrameLocator } from "@playwright/test";
import { waitForApp, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";

// freenet/river#1023: the admission proof of work runs in Web Workers, off the
// page's main thread. The fixture rooms (see public-room-join.spec.ts) are
// `open` (4 bits) and `hard` (26 bits: held by the holdSolver hook, or raised
// to 32, since Web Workers can solve 26 bits in seconds).

// Each join loads the app's wasm into several workers; a loaded machine (the
// suite runs projects in parallel) takes a while.
test.setTimeout(120_000);

async function hooksReady(page: Page) {
  await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
}

// A visitor's first send asks for a display name; Continue sends under the
// generated default (see public-room-join.spec.ts). A re-admitted member is
// not asked. Takes a page or a frame locator.
async function sendFirst(root: Page | FrameLocator) {
  await root.getByTestId("viewed-room-send").click();
  // Enter rather than a click: in the sandboxed iframe on a phone-sized page the
  // Continue button can be below the page's viewport.
  await root.getByTestId("viewed-room-nickname-input").press("Enter");
  await expect(root.getByTestId("viewed-room-nickname-modal")).toHaveCount(0);
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
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 60_000 });
  expect(await callRiverTest(page, "powSolverMode")).toBe("workers");
  // The page compiled the wasm once and handed the module to its workers.
  expect(await callRiverTest(page, "powModuleShared")).toBe(true);
  expect(await callRiverTest(page, "roomMessageCount", "From a worker")).toBe(1);
  // The solve is over: its workers are gone.
  await expect.poll(() => page.workers().length).toBe(0);
  expect(panics).toEqual([]);
});

test("cancelling a running solve stops its workers and keeps the draft", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  // The room's 26 bits are within a few seconds of four workers, so a test
  // that must not finish asks for the contract's maximum, 32 bits.
  expect(await callRiverTest(page, "retargetPolicy", "hard", 32, 2)).toBe(true);
  await page.getByTestId("viewed-room-composer").fill("Keep me");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
  // Workers go on in a background desktop tab; a phone may pause a hidden page.
  await expect(page.getByTestId("viewed-room-join-progress")).toContainText("Keep this tab open");
  // A padded percentage, in plain words.
  await expect(page.getByTestId("viewed-room-join-progress")).toContainText(/\d+% done/);
  await expect(page.getByTestId("viewed-room-join-progress")).not.toContainText(/proof|bot|bits|hash/i);
  // Really hashing (26 bits is out of reach of a test), in workers, not on the page.
  await expect.poll(() => callRiverTest(page, "powSolverMode"), { timeout: 60_000 }).toBe("workers");
  expect(page.workers().length).toBeGreaterThan(0);

  await page.getByTestId("viewed-room-join-cancel").click();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Keep me");
  await expect.poll(() => page.workers().length, { timeout: 10_000 }).toBe(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "Keep me")).toBe(0);
  expect(panics).toEqual([]);
});

test("cancelling while the workers are still loading leaves none running", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  expect(await callRiverTest(page, "retargetPolicy", "hard", 32, 2)).toBe(true);
  await page.getByTestId("viewed-room-composer").fill("Too early");
  await sendFirst(page);
  await page.getByTestId("viewed-room-join-cancel").click();
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Too early");
  // Whether the workers had loaded or not, none outlive the cancel.
  await expect.poll(() => page.workers().length, { timeout: 30_000 }).toBe(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "Too early")).toBe(0);
  expect(panics).toEqual([]);
});

test("a changed room policy retargets the workers and the message still posts once", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("After retarget");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
  // Retarget a solve that is running, not one still loading its workers.
  await expect.poll(() => callRiverTest(page, "powSolverMode"), { timeout: 60_000 }).toBe("workers");
  const before = await callRiverTest(page, "powRetargets");
  // More bits and a new epoch: the search restarts for the new policy.
  expect(await callRiverTest(page, "retargetPolicy", "open", 10, 2)).toBe(true);
  await expect.poll(() => callRiverTest(page, "powRetargets"), { timeout: 10_000 }).toBe(before + 1);
  // The wait is explained, not shown as an error.
  await expect(page.getByTestId("viewed-room-join-progress")).toContainText("The room asked for a longer wait");
  await callRiverTest(page, "holdSolver", false);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 60_000 });
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
  await sendFirst(page);
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
    const headers: Record<string, string> = { ...response.headers(), "access-control-allow-origin": "*" };
    // The policy the gateway serves with the app's document
    // (freenet-core `sandbox_csp_for_origin`).
    if (route.request().resourceType() === "document") {
      headers["content-security-policy"] =
        `sandbox allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-downloads allow-modals; ` +
        `default-src ${new URL(base).origin} 'unsafe-inline' 'unsafe-eval' blob: data:; connect-src ${new URL(base).origin} blob: data:`;
    }
    await route.fulfill({ response, headers });
  });
  await page.goto(`${base}/`);
  await waitForApp(page);
  await hooksReady(page);
  const key = await callRiverTest(page, "viewableRoomKey", "open");

  await page.setContent(
    `<iframe id="app" src="${base}/?room=${key}" sandbox="allow-scripts allow-forms allow-popups" style="width:1000px;height:700px"></iframe>`,
  );
  const frame = page.frameLocator("#app");
  await expect(frame.getByTestId("viewed-room-name")).toBeVisible({ timeout: 30_000 });
  await frame.getByTestId("viewed-room-composer").fill("Sandboxed");
  await sendFirst(frame);
  await expect(frame.getByTestId("viewed-room")).toHaveCount(0, { timeout: 60_000 });
  const handle = await page.locator("#app").elementHandle();
  const inner = (await handle!.contentFrame())!;
  const mode = await inner.evaluate(() => (window as any).__riverTest.powSolverMode());
  expect(mode).toBe("workers");
});

// The room's moderation raises the open-admission difficulty during an attack,
// which removes members whose proof is below it. An honest member loses
// nothing: the message they send next re-admits them at the new difficulty and
// posts exactly once, and the wait is explained, not reported as an error.
test("a member removed by a raised difficulty is re-admitted by their next message, which posts once", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await page.getByTestId("viewed-room-composer").fill("Joined");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 60_000 });

  expect(await callRiverTest(page, "raiseMemberRoomPolicy", "open", 12, 2)).toBe(true);
  // The room drops the entry whose proof is now too weak (the proof stays held here).
  expect(await callRiverTest(page, "pruneJoiner", false)).toBe(true);
  const idle = page.getByTestId("viewed-room-join-idle");
  await expect(idle).toContainText("Room security was raised");
  await expect(idle).not.toContainText("quiet");

  await callRiverTest(page, "holdSolver", true);
  const composer = page.getByTestId("viewed-room-composer");
  await composer.fill("After the raise");
  await page.getByTestId("viewed-room-send").click();
  const progress = page.getByTestId("viewed-room-join-progress");
  await expect(progress).toContainText("Room security was raised; confirming you're human");
  await expect(page.getByTestId("viewed-room-join-error")).toHaveCount(0);
  await expect(page.getByTestId("readmit-pending-message")).toContainText("After the raise");
  await callRiverTest(page, "holdSolver", false);

  await expect.poll(() => callRiverTest(page, "roomMessageCount", "After the raise"), { timeout: 60_000 }).toBe(1);
  // Not posted a second time, and nothing is left pending.
  await page.waitForTimeout(1500);
  expect(await callRiverTest(page, "roomMessageCount", "After the raise")).toBe(1);
  await expect(page.getByTestId("readmit-pending-message")).toHaveCount(0);
  expect(await callRiverTest(page, "powSolverMode")).toBe("workers");
  expect(panics).toEqual([]);
});

// A worker that cannot load its module must not make the join wait out the
// worker timeout, once or on every join: it falls back at once, the message
// posts exactly once, and later joins skip workers.
test("workers that cannot load fall back at once, post once, and are not tried again", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  // The page's own script has run; point the worker at a module that is not there.
  await page.evaluate(() => {
    for (const s of document.querySelectorAll<HTMLScriptElement>("script[type=module][src]")) {
      if (/river-ui/.test(s.src)) s.src = s.src.replace(/river-ui[^/]*\.js$/, "river-ui-missing.js");
    }
  });
  const began = Date.now();
  await page.getByTestId("viewed-room-composer").fill("No workers here");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 60_000 });
  expect(Date.now() - began, "fell back without waiting out the 30 s worker timeout").toBeLessThan(15_000);
  expect(await callRiverTest(page, "powSolverMode")).toBe("main");
  expect(await callRiverTest(page, "roomMessageCount", "No workers here")).toBe(1);
  await expect.poll(() => page.workers().length).toBe(0);
  expect(panics).toEqual([]);
});

test("a join after a worker failure goes straight to the main thread", async ({ page }) => {
  await openRoom(page, "open");
  await page.evaluate(() => {
    for (const s of document.querySelectorAll<HTMLScriptElement>("script[type=module][src]")) {
      if (/river-ui/.test(s.src)) s.src = s.src.replace(/river-ui[^/]*\.js$/, "river-ui-missing.js");
    }
  });
  await page.getByTestId("viewed-room-composer").fill("First");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 60_000 });
  // The same page joins another room: no worker is even started.
  await callRiverTest(page, "viewRoom", "hard");
  await page.getByTestId("viewed-room-name").waitFor();
  expect(await callRiverTest(page, "retargetPolicy", "hard", 4, 2)).toBe(true);
  let started = 0;
  page.on("worker", () => started++);
  await page.getByTestId("viewed-room-composer").fill("Second");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(started).toBe(0);
});
