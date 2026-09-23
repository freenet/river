import { test, expect } from "@playwright/test";

// The suite runs against a `no-sync` build served statically, which has no
// Freenet node behind it. Such a build must not try to reach one: it used to
// start the synchronizer anyway, open a WebSocket to the page's own origin,
// and (through freenet-stdlib's `onerror`, which reads `ErrorEvent::filename()`
// off a plain `Event`) raise an uncaught page error on every load.

// Not an exception: WebKit's notice that a ResizeObserver's notifications
// were carried over to the next frame. It surfaces here because a no-sync
// build reaches first layout before its stylesheets land, and the history's
// observer reads scroll position mid-delivery as the CSS applies. Benign (the
// notifications are delivered a frame later), and unrelated to sync.
const BENIGN = /ResizeObserver loop completed with undelivered notifications/;

test("a no-sync build opens no WebSocket and raises no page error", async ({ page }) => {
  const sockets: string[] = [];
  const errors: string[] = [];
  page.on("websocket", (ws) => sockets.push(ws.url()));
  page.on("pageerror", (e) => {
    if (!BENIGN.test(String(e))) errors.push(String(e));
  });

  await page.goto("/");
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  // The synchronizer used to start on mount and fail within milliseconds;
  // give any stray attempt ample time to show up.
  await page.waitForTimeout(1_500);

  expect(sockets, "no-sync build opened a WebSocket").toEqual([]);
  expect(errors, "uncaught page errors on load").toEqual([]);
  // With nothing to connect to, the status says so rather than "Connecting...".
  await expect(
    page.locator('[data-testid="connection-status-indicator"]:visible')
  ).toContainText("Disconnected");
});
