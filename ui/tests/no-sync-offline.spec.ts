import { test, expect } from "@playwright/test";
import { waitForApp } from "./example-room";

// Only the node API socket. CI serves the static release build, which opens no
// sockets; a local `dx serve` still adds its own hot-reload socket.
const NODE_SOCKET = "/v1/contract/command";

test("a no-sync build opens no node socket and starts Disconnected", { tag: "@chromium-only" }, async ({ page }) => {
  const sockets: string[] = [];
  page.on("websocket", (ws) => { if (ws.url().includes(NODE_SOCKET)) sockets.push(ws.url()); });

  await page.goto("/");
  await waitForApp(page);
  // Absence can't be awaited; a stray connect would fire within ms of mount.
  await page.waitForTimeout(1_500);

  expect(sockets, "no-sync build opened a node WebSocket").toEqual([]);
  await expect(page.locator('[data-testid="connection-status-indicator"]:visible')).toContainText("Disconnected");
});
