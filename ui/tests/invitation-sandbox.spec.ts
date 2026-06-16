import { test, expect, Page, FrameLocator } from "@playwright/test";

// Regression coverage for issue #217 (follow-up to the #215 fix shipped in
// PR #220).
//
// THE BUG (#215): a user who clicked a River invite link (`?invitation=<code>`)
// inside the Freenet gateway iframe saw the "Invitation Received" / choose-a-
// nickname modal pop up again on EVERY page reload. The gateway iframe runs
// with `sandbox="allow-scripts allow-forms allow-popups"` — note the absence of
// `allow-same-origin`. That gives the iframe an opaque origin, so it cannot:
//   * read/write `localStorage` (`window.localStorage` throws SecurityError), or
//   * rewrite its own URL to drop the `?invitation=` query
//     (`history.replaceState` requires same-origin).
// So the `?invitation=<code>` query survives every reload and, with no durable
// "already handled this invite" marker, the modal re-appeared each time.
//
// THE FIX (PR #220): River records a per-invitation fingerprint in River's slice
// of the *top-level* URL hash (`#river-processed=<fp>,<fp>,...`). The iframe
// can't write the top-level URL directly, so it asks the gateway shell to do it
// via the `{__freenet_shell__: true, type: 'hash', hash: '#...'}` postMessage
// bridge. On the next load the shell re-appends `location.hash` to the iframe
// `src`, so the iframe boots already knowing which invites were handled and the
// URL parser (`app.rs`) skips them via `is_invitation_processed`.
//   - mark:  receive_invitation_modal.rs::mark_invitation_processed -> persist_processed_list (postMessage)
//   - read:  receive_invitation_modal.rs::read_processed_from_window_hash (iframe's own location.hash)
//   - gate:  app.rs URL parser -> is_invitation_processed
//
// THIS TEST reproduces the failure mode end to end: it stands up a minimal
// "gateway shell" parent page that mirrors the two shell behaviours the fix
// depends on — (1) it appends its tracked top-level hash to the iframe `src` on
// every (re)load, and (2) it honours the iframe's `{type:'hash'}` postMessage by
// updating that tracked hash. It then opens an invite URL, accepts the invite,
// reloads with the SAME invite URL, and asserts the modal does NOT reappear and
// the processed fingerprint is present in the top-level hash. Without PR #220's
// hash-bridge dedup the second load would re-show the modal.
//
// Sandbox caveat (documented in AGENTS.md / project memory): the real gateway
// omits `allow-same-origin`, but a different-origin sandboxed iframe can't load
// WASM from the dev server, so we add `allow-same-origin` here. That does NOT
// weaken what we're testing: the dedup path goes through the postMessage shell
// bridge (parent !== self), NOT localStorage, in this fixture — exactly as it
// does in the opaque-origin gateway. The origin restriction is orthogonal to
// the fingerprint round-trip the regression is about.

// A frozen, valid invitation code. This is the byte-identical fixture pinned by
// `invitation_decodes_frozen_cross_side_fixture` in
// ui/src/components/members.rs (and the CLI counterpart in cli/src/api.rs), so
// it cannot silently drift from the real wire format. Its room is not in the
// example data, so the modal renders the fresh-invite ("Invitation Received" +
// nickname prompt) branch.
const INVITATION_CODE =
  "6DdkgteQ42ZdqjP42dauXJKUPV7Pb4YG5wxPzvBDezf3pwCkWX5ENtvTM8Eb9bVzDTG986W4SEY6MVx653EuNkBYhfTx7FM7uFHy3bJng5xoq8S6gfwuau9AgvWEixELwY7Pn9hErx6rymdPeBrpBouZgKkSLCbSqteJL3r1x8adRXkJVfDd8N9P1L9Uorah6J6sxisDuBcT3TZ71zmWaHkWwEptej7DUNUxCruLXjLGcJdWUaYP2YRAP5siqbNUz1rL9Jh5ZK7t8sq2p7WBSJasSyLuSJhDDw2qmRs5nGexupvbcimptn1xQBdzNa6q3bgzt8Qka3Ror5AD7iN6UNpGQPqwgrmvX6g8q2zVMDKh1JeEP9tezNtpmige3WvwRMg2wKk7pFnLNaeGyutEVQrsrd73D9TsB1Mkz86WwxMU8pKvonLgr2TB9yJdiX1BBkDPRZ6yE2bEzxyeo3PZ6t9Nw4WVszSBnFDkAKzAnCoHdo9qpm6n4iY5R6rsANPn75WDiUM16UyqzVsYdWH2JhoVuvpz7D8HUgbGcjTDsMxi33aERdtd7vG24oDMMsKYYNP6VGdXfyRWKm7LUk9M1hFyD1Sf9FZksUxpp924mRNyaJUCniR9pY984jDUrNE3gCuK1PoF9ShtCvEd";

// The dev server River is served from. We deliberately read the bare origin so
// the iframe `src` we build matches what the gateway constructs: data-src plus
// `?invitation=...` plus `#...` hash.
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:8082";

// Match the gateway's sandbox attributes, plus allow-same-origin so the dev
// server's WASM can load (see the sandbox caveat in the file header).
const SANDBOX_ATTRS =
  "allow-scripts allow-forms allow-popups allow-same-origin";

// A minimal stand-in for the gateway's SHELL_BRIDGE_JS hash handling
// (freenet-core crates/core/src/server/path_handlers.rs). It implements ONLY
// the two behaviours the #220 fix relies on:
//   1. On (re)load it appends the tracked top-level hash to the iframe src,
//      exactly like `iframeSrc += location.hash` in the shell.
//   2. It honours the iframe's `{__freenet_shell__, type:'hash', hash}`
//      postMessage by recording the new hash (the shell does
//      `history.replaceState(history.state, '', h)`).
// The tracked hash lives on `window.__shellHash` so the test can drive a
// "reload" by calling `window.__shellReload()` (rebuild the iframe with the
// current tracked hash) and can read the persisted hash back for assertions.
const SHELL_HTML = (src: string) => `<!DOCTYPE html>
<html>
<head><meta charset="utf-8"></head>
<body style="margin:0;padding:0;height:100vh;">
  <script>
    // Tracked top-level hash, the analog of the real shell's location.hash.
    window.__shellHash = "";
    var INVITE_SRC = ${JSON.stringify(src)};

    function buildIframe() {
      var existing = document.getElementById("river-frame");
      if (existing) existing.remove();
      var iframe = document.createElement("iframe");
      iframe.id = "river-frame";
      iframe.setAttribute("sandbox", ${JSON.stringify(SANDBOX_ATTRS)});
      iframe.style.cssText = "width:100%;height:100%;border:none;";
      // Mirror the shell: append the tracked top-level hash to the iframe src.
      iframe.src = INVITE_SRC + (window.__shellHash || "");
      document.body.appendChild(iframe);
      return iframe;
    }

    // Mirror the shell's postMessage hash handler. Only accept messages from
    // our own iframe and only hash-type shell messages whose payload starts
    // with '#'. Record the new top-level hash (replaceState analog).
    window.addEventListener("message", function (event) {
      var frame = document.getElementById("river-frame");
      if (!frame || event.source !== frame.contentWindow) return;
      var msg = event.data;
      if (!msg || !msg.__freenet_shell__) return;
      if (msg.type === "hash" && typeof msg.hash === "string") {
        var h = msg.hash.slice(0, 8192);
        if (h === "#") {
          // Shell collapses a lone '#' to an empty hash.
          window.__shellHash = "";
        } else if (h.length > 0 && h.charAt(0) === "#") {
          window.__shellHash = h;
        }
      }
    });

    // Test hook: rebuild the iframe from scratch with the current tracked
    // hash — the durable-reload analog (a real reload re-runs the shell, which
    // re-appends location.hash to the iframe src).
    window.__shellReload = function () { buildIframe(); };

    buildIframe();
  </script>
</body>
</html>`;

async function waitForRiverApp(frame: FrameLocator) {
  await frame.locator(".app-root").waitFor({ timeout: 30_000 });
}

function inviteSrc(): string {
  // No trailing slash assumptions: River's URL parser reads
  // window.location.search for `invitation`, independent of path.
  return `${BASE}/?invitation=${INVITATION_CODE}`;
}

const modalHeading = (frame: FrameLocator) =>
  frame.getByRole("heading", { name: "Invitation Received" });

test.describe("Invitation flow under gateway-style sandboxed iframe (#217)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("invite modal appears once, then is suppressed on reload after accept", async ({
    page,
  }) => {
    await page.setContent(SHELL_HTML(inviteSrc()));
    const frame = page.frameLocator("#river-frame");
    await waitForRiverApp(frame);

    // 1. First load: the modal must appear for the fresh invite.
    await expect(modalHeading(frame)).toBeVisible({ timeout: 15_000 });
    await expect(
      frame.getByText("You have been invited to join a new room.")
    ).toBeVisible();

    // The top-level hash starts clean — nothing processed yet.
    expect(await page.evaluate(() => window.__shellHash)).toBe("");

    // 2. Decline the invite. Any definitive action (Accept/Decline/Close)
    //    marks it processed via the shell hash bridge; Decline is the
    //    deterministic choice because it doesn't depend on a (no-sync) room
    //    subscription ever completing. The fix records the fingerprint at the
    //    same place for every dismiss path
    //    (`dismiss_invitation_persistently`).
    await frame.getByRole("button", { name: "Decline" }).click();

    // Modal closes, and the shell receives a `#river-processed=...` hash via
    // the postMessage bridge. Poll the tracked hash rather than sleeping.
    await expect(modalHeading(frame)).toBeHidden({ timeout: 10_000 });
    await expect
      .poll(() => page.evaluate(() => window.__shellHash), {
        timeout: 10_000,
      })
      .toMatch(/^#river-processed=[0-9a-f]/);

    // 3. Reload with the SAME invite URL. The shell re-appends the tracked
    //    top-level hash to the iframe src, exactly as the gateway does.
    await page.evaluate(() => window.__shellReload());
    const frame2 = page.frameLocator("#river-frame");
    await waitForRiverApp(frame2);

    // 4. The regression assertion: the modal must NOT reappear, because the
    //    invitation's fingerprint is already in the propagated hash and
    //    `is_invitation_processed` short-circuits the URL parser. Give the app
    //    time to (incorrectly, pre-fix) surface the modal — if it were going
    //    to re-prompt, it would by the time the welcome screen has rendered.
    await expect(
      frame2.getByText("Welcome to River")
    ).toBeVisible({ timeout: 15_000 });
    await expect(modalHeading(frame2)).toHaveCount(0);

    // The fingerprint survived the reload in the top-level hash.
    expect(await page.evaluate(() => window.__shellHash)).toMatch(
      /^#river-processed=[0-9a-f]/
    );
  });

  test("invite modal still appears on reload BEFORE any action (no premature suppression)", async ({
    page,
  }) => {
    // Guards the other half of the fix's contract: the fingerprint is recorded
    // only on a definitive user action, NOT at URL-parse time. A user who
    // reloads before deciding must still see the modal — otherwise a stray
    // click or refresh would lock them out of an invite they never acted on
    // (the UX regression the #220 author called out in
    // `mark_invitation_processed`'s docs).
    await page.setContent(SHELL_HTML(inviteSrc()));
    const frame = page.frameLocator("#river-frame");
    await waitForRiverApp(frame);
    await expect(modalHeading(frame)).toBeVisible({ timeout: 15_000 });

    // No action taken. The top-level hash must remain empty.
    expect(await page.evaluate(() => window.__shellHash)).toBe("");

    // Reload with the same URL and no recorded fingerprint.
    await page.evaluate(() => window.__shellReload());
    const frame2 = page.frameLocator("#river-frame");
    await waitForRiverApp(frame2);

    // Modal must reappear — the invite was never processed.
    await expect(modalHeading(frame2)).toBeVisible({ timeout: 15_000 });
  });
});

declare global {
  interface Window {
    __shellHash: string;
    __shellReload: () => void;
  }
}
