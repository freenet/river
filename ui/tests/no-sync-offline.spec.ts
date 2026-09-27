import { test, expect } from "@playwright/test";
import { waitForApp } from "./example-room";

// WebKit notice that ResizeObserver notifications slipped a frame; benign and unrelated to sync.
const BENIGN = /ResizeObserver loop completed with undelivered notifications/;
// Only the node API socket: `dx serve` (CI) adds its own hot-reload socket.
const NODE_SOCKET = "/v1/contract/command";

test("a no-sync build opens no node socket and raises no page error", { tag: "@chromium-only" }, async ({ page }) => {
  const sockets: string[] = [];
  const errors: string[] = [];
  page.on("websocket", (ws) => { if (ws.url().includes(NODE_SOCKET)) sockets.push(ws.url()); });
  page.on("pageerror", (e) => { if (!BENIGN.test(String(e))) errors.push(String(e)); });

  await page.goto("/");
  await waitForApp(page);
  // A stray connect happened within milliseconds of mount; absence needs a short wait.
  await page.waitForTimeout(1_500);

  expect(sockets, "no-sync build opened a node WebSocket").toEqual([]);
  expect(errors, "uncaught page errors").toEqual([]);
  await expect(page.locator('[data-testid="connection-status-indicator"]:visible')).toContainText("Disconnected");
});
