import { test, expect } from "@playwright/test";
import { waitForApp } from "./example-room";

// A damaged invitation link must say so. Field report: a phone opened an
// invite link whose `?invitation=` code had been cut to 298 of 478 characters,
// and River showed only an empty room list, with no dialog and no error.

// The frozen, valid invitation fixture (see invitation-sandbox.spec.ts and
// `invitation_decodes_frozen_cross_side_fixture` in ui/src/components/members.rs).
const INVITATION_CODE =
  "6DdkgteQ42ZdqjP42dauXJKUPV7Pb4YG5wxPzvBDezf3pwCkWX5ENtvTM8Eb9bVzDTG986W4SEY6MVx653EuNkBYhfTx7FM7uFHy3bJng5xoq8S6gfwuau9AgvWEixELwY7Pn9hErx6rymdPeBrpBouZgKkSLCbSqteJL3r1x8adRXkJVfDd8N9P1L9Uorah6J6sxisDuBcT3TZ71zmWaHkWwEptej7DUNUxCruLXjLGcJdWUaYP2YRAP5siqbNUz1rL9Jh5ZK7t8sq2p7WBSJasSyLuSJhDDw2qmRs5nGexupvbcimptn1xQBdzNa6q3bgzt8Qka3Ror5AD7iN6UNpGQPqwgrmvX6g8q2zVMDKh1JeEP9tezNtpmige3WvwRMg2wKk7pFnLNaeGyutEVQrsrd73D9TsB1Mkz86WwxMU8pKvonLgr2TB9yJdiX1BBkDPRZ6yE2bEzxyeo3PZ6t9Nw4WVszSBnFDkAKzAnCoHdo9qpm6n4iY5R6rsANPn75WDiUM16UyqzVsYdWH2JhoVuvpz7D8HUgbGcjTDsMxi33aERdtd7vG24oDMMsKYYNP6VGdXfyRWKm7LUk9M1hFyD1Sf9FZksUxpp924mRNyaJUCniR9pY984jDUrNE3gCuK1PoF9ShtCvEd";

const MESSAGE = "This invitation link looks incomplete or damaged.";

const DAMAGED: Array<[string, string]> = [
  ["truncated", INVITATION_CODE.slice(0, Math.floor((INVITATION_CODE.length * 298) / 478))],
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
