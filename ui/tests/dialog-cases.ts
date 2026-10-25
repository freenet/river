import { expect, Locator, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";

// The dialogs a viewer can reach from the example data, each with how to
// open it from a room, its title, and its primary action where it has a
// neutral one (destructive confirmations keep their status colour).

/// A real invitation for `/?invitation=`, from the example build.
export const INVITATION_CODE =
  "6DdkgteQ42ZdqjP42dauXJKUPV7Pb4YG5wxPzvBDezf3pwCkWX5ENtvTM8Eb9bVzDTG986W4SEY6MVx653EuNkBYhfTx7FM7uFHy3bJng5xoq8S6gfwuau9AgvWEixELwY7Pn9hErx6rymdPeBrpBouZgKkSLCbSqteJL3r1x8adRXkJVfDd8N9P1L9Uorah6J6sxisDuBcT3TZ71zmWaHkWwEptej7DUNUxCruLXjLGcJdWUaYP2YRAP5siqbNUz1rL9Jh5ZK7t8sq2p7WBSJasSyLuSJhDDw2qmRs5nGexupvbcimptn1xQBdzNa6q3bgzt8Qka3Ror5AD7iN6UNpGQPqwgrmvX6g8q2zVMDKh1JeEP9tezNtpmige3WvwRMg2wKk7pFnLNaeGyutEVQrsrd73D9TsB1Mkz86WwxMU8pKvonLgr2TB9yJdiX1BBkDPRZ6yE2bEzxyeo3PZ6t9Nw4WVszSBnFDkAKzAnCoHdo9qpm6n4iY5R6rsANPn75WDiUM16UyqzVsYdWH2JhoVuvpz7D8HUgbGcjTDsMxi33aERdtd7vG24oDMMsKYYNP6VGdXfyRWKm7LUk9M1hFyD1Sf9FZksUxpp924mRNyaJUCniR9pY984jDUrNE3gCuK1PoF9ShtCvEd";

export type DialogCase = {
  name: string;
  /// The page to load ("/" unless the dialog opens from a URL).
  path?: string;
  /// The room to open first, if any.
  room?: string;
  open: (page: Page) => Promise<void>;
  title: (page: Page) => Locator;
  /// The dialog's neutral primary action, if it has one.
  primary?: (page: Page) => Locator;
};

async function openMemberInfo(page: Page) {
  const rows = page.getByTestId("member-list").locator('[data-testid^="member-item-"] button');
  await rows.filter({ hasNot: page.getByTestId("member-list-self") }).first().click();
  await expect(page.getByTestId("member-info-modal")).toBeVisible();
}

async function openDm(page: Page) {
  await callRiverTest(page, "appendDmsForPeer", 0, 3);
  await page.locator(".dm-rail-row-btn", { hasText: "DM Test Peer" }).click();
  await expect(page.getByTestId("dm-thread-modal")).toBeVisible();
}

export const DIALOG_CASES: DialogCase[] = [
  {
    name: "create room",
    open: (p) => p.getByTestId("create-room-button").click(),
    title: (p) => p.getByRole("heading", { name: "Create New Room", exact: true }),
    primary: (p) => p.getByTestId("create-room-submit-button"),
  },
  {
    name: "invite member",
    room: "Your Private Room",
    open: (p) => p.getByTestId("invite-member-button").click(),
    title: (p) => p.getByRole("heading", { name: "Invite Member", exact: true }),
  },
  {
    name: "enter invite code",
    open: (p) => p.getByTestId("join-with-code-button").click(),
    title: (p) => p.getByRole("heading", { name: "Enter Invite Code", exact: true }),
    primary: (p) => p.getByTestId("join-with-code-submit-button"),
  },
  {
    name: "import ID",
    room: "Your Private Room",
    open: (p) => p.getByTestId("import-id-button").click(),
    title: (p) => p.getByRole("heading", { name: "Import Identity", exact: true }),
    primary: (p) => p.getByTestId("import-identity-submit"),
  },
  {
    name: "export ID",
    room: "Your Private Room",
    open: (p) => p.getByTestId("export-id-button").click(),
    title: (p) => p.getByRole("heading", { name: "Export Identity", exact: true }),
    primary: (p) => p.getByRole("button", { name: "Copy to Clipboard" }),
  },
  {
    name: "member info",
    room: "Team Chat Room",
    open: openMemberInfo,
    title: (p) => p.getByRole("heading", { name: "Member Info", exact: true }),
  },
  {
    name: "room details",
    room: "Your Private Room",
    open: (p) => p.getByTestId("room-info-button").click(),
    title: (p) => p.getByTestId("edit-room-modal").getByRole("heading").first(),
  },
  {
    name: "notifications",
    room: "Team Chat Room",
    open: (p) => p.getByTestId("notification-bell-button").click(),
    title: (p) => p.getByTestId("notification-modal").getByRole("heading").first(),
  },
  {
    name: "receive invitation",
    path: "/?invitation=" + INVITATION_CODE,
    open: async () => {},
    title: (p) => p.getByTestId("receive-invitation-modal").getByRole("heading").first(),
    primary: (p) => p.getByTestId("receive-invitation-accept-button"),
  },
  {
    name: "confirm ban",
    room: "Your Private Room",
    open: async (p) => {
      await openMemberInfo(p);
      await p.getByRole("button", { name: "Ban User" }).click();
    },
    title: (p) => p.getByRole("heading", { name: "Confirm Ban", exact: true }),
  },
  {
    name: "DM thread",
    room: "Team Chat Room",
    open: openDm,
    title: (p) => p.getByTestId("dm-thread-modal").getByRole("heading").first(),
  },
  {
    name: "invite via DM picker",
    room: "Team Chat Room",
    open: async (p) => {
      await openMemberInfo(p);
      await p.getByTestId("member-info-share-invite-button").click();
    },
    title: (p) => p.getByRole("heading", { name: /invite .+ to another room/i }),
    primary: (p) => p.getByRole("button", { name: "Send invite" }),
  },
  {
    name: "delete message",
    room: "Your Private Room",
    open: async (p) => {
      const own = p.locator('[data-testid="message-row"][data-message-self="true"]').last();
      await own.scrollIntoViewIfNeeded();
      await own.hover({ position: { x: 4, y: 4 } });
      await own.getByTestId("message-hover-actions").getByRole("button", { name: "Delete message" }).click();
    },
    title: (p) => p.getByRole("heading", { name: /Delete Message/ }),
  },
];
