import { test, expect, FrameLocator } from "@playwright/test";

// Regression coverage for issue #510, in the shape the bug actually occurs:
// River running framed inside the gateway shell.
//
// THE BUG: River's shell-bridge listener dropped every message whose `type` was
// not `notification_click`. The shell reports the outcome of its permission
// offer as `{__freenet_shell__: true, type: 'notification_status', status}`
// (`notifyStatusToIframe` in freenet-core's `shell_bridge.js`), with `granted` /
// `denied` / `dismissed` / `undeliverable` / `unsupported` / `default`. All of
// them were discarded identically, so River could not tell a granted permission
// from a blocked one and surfaced nothing. A one-way `ENABLE_PROMPT_SENT` latch
// then meant a blocked or mis-clicked prompt could never be re-requested for the
// rest of the session.
//
// THIS TEST stands up a minimal "gateway shell" parent page (the same fixture
// shape as invitation-sandbox.spec.ts) that does the two things the fix talks
// to: it can post a `notification_status` down to the iframe, and it records the
// `notification_enable_prompt` messages the iframe posts up. It then drives the
// bell modal through the real statuses and asserts what the user is told, and
// that the manual retry actually reaches the shell.
//
// The unit tests in `notifications.rs` pin the decisions; this pins that they
// are wired to a real postMessage from a real parent frame — the connection no
// native test in a WASM crate can reach.
//
// Sandbox caveat, identical to invitation-sandbox.spec.ts: the real gateway
// omits `allow-same-origin`, but a different-origin sandboxed iframe cannot load
// WASM from the dev server, so it is added here. It does not weaken what is
// under test — the path exercised is the postMessage bridge (parent !== self),
// exactly as in the opaque-origin gateway.

const BASE = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:8082";

const SANDBOX_ATTRS =
  "allow-scripts allow-forms allow-popups allow-same-origin";

const ROOM = "Public Discussion Room";

// Minimal stand-in for the gateway shell. Implements only the two notification
// behaviours the fix depends on:
//   1. `window.__postStatus(status)` posts a `notification_status` to the iframe,
//      byte-identical to the real shell's `notifyStatusToIframe`.
//   2. It records every `notification_enable_prompt` the iframe posts up, on
//      `window.__prompts`, so a test can assert the manual retry got through.
const SHELL_HTML = `<!DOCTYPE html>
<html>
<head><meta charset="utf-8"></head>
<body style="margin:0;padding:0;height:100vh;">
  <script>
    window.__prompts = 0;

    var iframe = document.createElement("iframe");
    iframe.id = "river-frame";
    iframe.setAttribute("sandbox", ${JSON.stringify(SANDBOX_ATTRS)});
    iframe.style.cssText = "width:100%;height:100%;border:none;";
    iframe.src = ${JSON.stringify(BASE + "/")};
    document.body.appendChild(iframe);

    // The shell's half of the bridge: count the app's requests to be offered
    // notifications.
    window.addEventListener("message", function (event) {
      if (event.source !== iframe.contentWindow) return;
      var msg = event.data;
      if (!msg || !msg.__freenet_shell__) return;
      if (msg.type === "notification_enable_prompt") window.__prompts++;
    });

    // Mirror notifyStatusToIframe exactly.
    window.__postStatus = function (status) {
      iframe.contentWindow.postMessage(
        { __freenet_shell__: true, type: "notification_status", status: status },
        "*"
      );
    };
  </script>
</body>
</html>`;

const message = (frame: FrameLocator) =>
  frame.getByTestId("notification-permission-message");
const enableButton = (frame: FrameLocator) =>
  frame.getByTestId("notification-enable-button");

test.describe("Shell notification_status handling (#510)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the shell's permission outcome reaches the UI, and the retry reaches the shell", async ({
    page,
  }) => {
    await page.setContent(SHELL_HTML);
    const frame = page.frameLocator("#river-frame");
    await frame.locator(".app-root").waitFor({ timeout: 30_000 });

    // Open the bell modal for a room.
    await frame.getByRole("button", { name: ROOM }).click();
    await expect(frame.getByRole("heading", { name: ROOM })).toBeVisible({
      timeout: 10_000,
    });
    await frame.getByTestId("notification-bell-button").click();
    await expect(frame.getByTestId("notification-modal")).toBeVisible({
      timeout: 10_000,
    });

    // Framed with nothing reported yet: the shell is the only possible source,
    // so River says so and offers to ask.
    await expect(message(frame)).toBeVisible();
    await expect(enableButton(frame)).toBeVisible();

    // A blocked permission must be explained, and must NOT offer a button that
    // cannot change the answer. Before the fix this status was discarded and the
    // user saw nothing at all.
    await page.evaluate(() => window.__postStatus("denied"));
    await expect(message(frame)).toContainText(/blocking/i, { timeout: 10_000 });
    await expect(enableButton(frame)).toHaveCount(0);

    // A status River doesn't recognise must be IGNORED, not guessed at: it must
    // not overwrite the state River already has.
    //
    // This is a NON-event, so it needs a window rather than a wait. An earlier
    // version asserted `toContainText(/blocking/i)` immediately after posting,
    // which passed on its first poll before the message could possibly have
    // been processed — it would have passed just as well if the junk DID
    // clobber. The write path is a single `setTimeout(0)`, so a clobber renders
    // well inside this window.
    await page.evaluate(() => window.__postStatus("not-a-real-status"));
    await page.waitForTimeout(500);
    await expect(message(frame)).toContainText(/blocking/i);
    await expect(enableButton(frame)).toHaveCount(0);

    // ...and the listener is still alive after ignoring it. Without this, a
    // parse that THREW and killed the listener would look identical to
    // correctly ignoring the junk: both leave the UI sitting on "blocking".
    // Granting also flips the explanation and keeps the button away (there is
    // nothing left to ask for).
    await page.evaluate(() => window.__postStatus("granted"));
    await expect(message(frame)).toContainText(/enabled/i, { timeout: 10_000 });
    await expect(enableButton(frame)).toHaveCount(0);

    // A dismissal is recoverable, so the retry comes back...
    await page.evaluate(() => window.__postStatus("dismissed"));
    await expect(enableButton(frame)).toBeVisible({ timeout: 10_000 });

    // ...and clicking it actually asks the shell again. This is the half of
    // #510 that the once-per-session latch made impossible: River had already
    // sent its one prompt, so nothing reached the shell.
    const before = await page.evaluate(() => window.__prompts);
    await enableButton(frame).click();
    await expect
      .poll(() => page.evaluate(() => window.__prompts), { timeout: 10_000 })
      .toBeGreaterThan(before);

    // A second click asks again: the manual path is deliberately un-latched.
    const after = await page.evaluate(() => window.__prompts);
    await enableButton(frame).click();
    await expect
      .poll(() => page.evaluate(() => window.__prompts), { timeout: 10_000 })
      .toBeGreaterThan(after);
  });

  // The half of "fails silently" that the modal alone does not close: a user
  // with a blocked permission has no reason to open the modal, so the bell
  // itself has to say something.
  test("a blocked permission shows on the bell without opening anything", async ({
    page,
  }) => {
    await page.setContent(SHELL_HTML);
    const frame = page.frameLocator("#river-frame");
    await frame.locator(".app-root").waitFor({ timeout: 30_000 });

    await frame.getByRole("button", { name: ROOM }).click();
    await expect(frame.getByRole("heading", { name: ROOM })).toBeVisible({
      timeout: 10_000,
    });

    const badge = frame.getByTestId("notification-blocked-badge");
    const bell = frame.getByTestId("notification-bell-button");

    // Nothing reported yet: nothing to warn about.
    await expect(bell).toBeVisible();
    await expect(badge).toHaveCount(0);

    await page.evaluate(() => window.__postStatus("denied"));
    await expect(badge).toBeVisible({ timeout: 10_000 });

    // Not colour-only — the reason is in the accessible name, and the tooltip
    // still names just the per-room mode.
    await expect(bell).toHaveAttribute("aria-label", /not delivering/i);
    await expect(bell).toHaveAttribute("title", "Notifications: All messages");

    // And it clears when delivery starts working, rather than sticking.
    await page.evaluate(() => window.__postStatus("granted"));
    await expect(badge).toHaveCount(0, { timeout: 10_000 });
    await expect(bell).toHaveAttribute("aria-label", "Notifications: All messages");
  });

  test("a browser that cannot display notifications says so instead of failing silently", async ({
    page,
  }) => {
    await page.setContent(SHELL_HTML);
    const frame = page.frameLocator("#river-frame");
    await frame.locator(".app-root").waitFor({ timeout: 30_000 });

    await frame.getByRole("button", { name: ROOM }).click();
    await expect(frame.getByRole("heading", { name: ROOM })).toBeVisible({
      timeout: 10_000,
    });
    await frame.getByTestId("notification-bell-button").click();
    await expect(frame.getByTestId("notification-modal")).toBeVisible({
      timeout: 10_000,
    });

    // Permission exists but nothing could show the notification. The user is
    // told the in-app surfaces still work, and is not offered a re-ask that
    // would change nothing.
    await page.evaluate(() => window.__postStatus("undeliverable"));
    await expect(message(frame)).toContainText(/couldn't display/i, {
      timeout: 10_000,
    });
    await expect(enableButton(frame)).toHaveCount(0);

    await page.evaluate(() => window.__postStatus("unsupported"));
    await expect(message(frame)).toContainText(/doesn't support/i, {
      timeout: 10_000,
    });
    await expect(enableButton(frame)).toHaveCount(0);
  });
});

declare global {
  interface Window {
    __postStatus: (status: string) => void;
    __prompts: number;
  }
}
