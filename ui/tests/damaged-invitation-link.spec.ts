import { test, expect, Page } from "@playwright/test";
import { waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// A damaged invitation link must say so. Field report: a phone opened an
// invite link whose `?invitation=` code had been cut to 298 of 478 characters,
// and River showed only an empty room list, with no dialog and no error.

// The frozen, valid invitation fixture (see invitation-sandbox.spec.ts and
// `invitation_decodes_frozen_cross_side_fixture` in ui/src/components/members.rs).
const INVITATION_CODE =
  "6DdkgteQ42ZdqjP42dauXJKUPV7Pb4YG5wxPzvBDezf3pwCkWX5ENtvTM8Eb9bVzDTG986W4SEY6MVx653EuNkBYhfTx7FM7uFHy3bJng5xoq8S6gfwuau9AgvWEixELwY7Pn9hErx6rymdPeBrpBouZgKkSLCbSqteJL3r1x8adRXkJVfDd8N9P1L9Uorah6J6sxisDuBcT3TZ71zmWaHkWwEptej7DUNUxCruLXjLGcJdWUaYP2YRAP5siqbNUz1rL9Jh5ZK7t8sq2p7WBSJasSyLuSJhDDw2qmRs5nGexupvbcimptn1xQBdzNa6q3bgzt8Qka3Ror5AD7iN6UNpGQPqwgrmvX6g8q2zVMDKh1JeEP9tezNtpmige3WvwRMg2wKk7pFnLNaeGyutEVQrsrd73D9TsB1Mkz86WwxMU8pKvonLgr2TB9yJdiX1BBkDPRZ6yE2bEzxyeo3PZ6t9Nw4WVszSBnFDkAKzAnCoHdo9qpm6n4iY5R6rsANPn75WDiUM16UyqzVsYdWH2JhoVuvpz7D8HUgbGcjTDsMxi33aERdtd7vG24oDMMsKYYNP6VGdXfyRWKm7LUk9M1hFyD1Sf9FZksUxpp924mRNyaJUCniR9pY984jDUrNE3gCuK1PoF9ShtCvEd";

const MESSAGE = "This invitation link looks incomplete or damaged.";

const TRUNCATED = INVITATION_CODE.slice(0, Math.floor((INVITATION_CODE.length * 298) / 478));

const DAMAGED: Array<[string, string]> = [
  ["truncated", TRUNCATED],
  ["garbage", "notarealinvitation"],
  ["empty", ""],
];

for (const [name, code] of DAMAGED) {
  test(`a ${name} invitation code shows the damaged-link message`, async ({ page }) => {
    await page.goto(`/?invitation=${code}`);
    await waitForApp(page);
    const modal = page.getByTestId("damaged-invitation-modal");
    await expect(modal).toBeVisible();
    await expect(modal).toContainText(MESSAGE);
    await expect(page.getByTestId("receive-invitation-modal")).toHaveCount(0);

    await page.getByTestId("damaged-invitation-dismiss-button").click();
    await expect(modal).toHaveCount(0);
  });
}

test("a valid invitation code still opens the invitation dialog", async ({ page }) => {
  await page.goto(`/?invitation=${INVITATION_CODE}`);
  await waitForApp(page);
  await expect(page.getByTestId("receive-invitation-modal")).toBeVisible();
  await expect(page.getByTestId("damaged-invitation-modal")).toHaveCount(0);
});

test("no invitation parameter shows no damaged-link message", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await expect(page.getByTestId("damaged-invitation-modal")).toHaveCount(0);
});

// The Freenet gateway runs River in a sandboxed iframe that cannot rewrite its
// own address, so `?invitation=` stays in the URL for the whole session and
// across reloads. Reproduce that here by making `history.replaceState` fail.
async function keepInvitationInUrl(page: Page) {
  await page.addInitScript(() => {
    history.replaceState = () => {
      throw new Error("sandboxed: replaceState unavailable");
    };
  });
}

// Re-render `App`, whose body re-reads `?invitation=` on every render.
async function rerenderApp(page: Page) {
  await page.waitForFunction(() => typeof (window as any).__riverTest?.setMobileView === "function");
  await callRiverTest(page, "setMobileView", "chat");
  await callRiverTest(page, "setMobileView", "rooms");
  await callRiverTest(page, "setMobileView", "chat");
}

test("a dismissed damaged-link message stays closed when River re-renders", async ({ page }) => {
  await keepInvitationInUrl(page);
  await page.goto(`/?invitation=${TRUNCATED}`);
  await waitForApp(page);
  const modal = page.getByTestId("damaged-invitation-modal");
  await expect(modal).toBeVisible();
  await page.getByTestId("damaged-invitation-dismiss-button").click();
  await expect(modal).toHaveCount(0);
  await rerenderApp(page);
  // Give a reopened message time to paint before asserting it is absent.
  await page.waitForTimeout(500);
  await expect(modal).toHaveCount(0);
});

test("a dismissed damaged-link message stays closed after a reload", async ({ page }) => {
  await keepInvitationInUrl(page);
  await page.goto(`/?invitation=${TRUNCATED}`);
  await waitForApp(page);
  const modal = page.getByTestId("damaged-invitation-modal");
  await expect(modal).toBeVisible();
  await page.getByTestId("damaged-invitation-dismiss-button").click();
  await expect(modal).toHaveCount(0);
  // The dismissal is recorded in River's slice of the address hash.
  await expect.poll(() => page.evaluate(() => location.hash)).toContain("river-processed=");
  await page.reload();
  await waitForApp(page);
  await page.waitForTimeout(500);
  await expect(modal).toHaveCount(0);
});

test("the damaged-link message is an accessible dialog: Escape, backdrop, 44px target", async ({ page }) => {
  await page.goto(`/?invitation=${TRUNCATED}`);
  await waitForApp(page);
  const modal = page.getByTestId("damaged-invitation-modal");
  await expect(modal).toBeVisible();
  await expect(modal).toHaveAttribute("role", "alertdialog");
  await expect(modal).toHaveAttribute("aria-modal", "true");
  await expect(page.getByRole("alertdialog", { name: "Can't open this invitation" })).toBeVisible();
  const box = await page.getByTestId("damaged-invitation-dismiss-button").boundingBox();
  expect(box!.height).toBeGreaterThanOrEqual(44);

  await expect(page.getByTestId("damaged-invitation-dismiss-button")).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(modal).toHaveCount(0);
});

test("clicking outside the damaged-link message dismisses it", async ({ page }) => {
  await page.goto("/?invitation=notarealinvitation");
  await waitForApp(page);
  const modal = page.getByTestId("damaged-invitation-modal");
  await expect(modal).toBeVisible();
  await modal.click();
  await expect(modal).toBeVisible();
  await page.mouse.click(4, 4);
  await expect(modal).toHaveCount(0);
});

// In-app invite links are intercepted only when River is served at its
// gateway path, so serve the build there for these tests.
const RIVER_ID = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
const RIVER_PATH = `/v1/contract/web/${RIVER_ID}/`;

async function openAtGatewayPath(page: Page) {
  await page.route(`**${RIVER_PATH}*`, async (route) => {
    const root = new URL("/", route.request().url()).toString();
    await route.fulfill({ response: await route.fetch({ url: root }) });
  });
  await page.goto(RIVER_PATH);
  await waitForApp(page);
}

async function clickInviteLink(page: Page, code: string) {
  await page.evaluate((href) => {
    const a = document.createElement("a");
    a.href = href;
    a.textContent = "invite link";
    a.setAttribute("data-testid", "injected-invite-link");
    a.style.cssText = "position:fixed;top:0;left:0;z-index:9999;padding:8px;background:white";
    document.body.appendChild(a);
  }, `${RIVER_PATH}?invitation=${code}`);
  await page.getByTestId("injected-invite-link").click();
}

for (const [name, code] of [
  ["truncated", TRUNCATED],
  ["empty", ""],
] as Array<[string, string]>) {
  test(`clicking a ${name} invitation link in River shows the damaged-link message in place`, async ({ page }) => {
    await openAtGatewayPath(page);
    await clickInviteLink(page, code);
    await expect(page.getByTestId("damaged-invitation-modal")).toBeVisible();
    // Intercepted: the page did not navigate to the link.
    expect(new URL(page.url()).search).toBe("");
  });
}

test("clicking a valid invitation link in River still opens the invitation dialog", async ({ page }) => {
  await openAtGatewayPath(page);
  await clickInviteLink(page, INVITATION_CODE);
  await expect(page.getByTestId("receive-invitation-modal")).toBeVisible();
  await expect(page.getByTestId("damaged-invitation-modal")).toHaveCount(0);
});
