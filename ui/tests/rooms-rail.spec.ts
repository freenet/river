import { test, expect } from "@playwright/test";
import { resolveColor, selectRoom, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// The left rail: a brand row, sentence-case section labels, the open room
// raised and semibold (not blue), accent-soft unread pills, DM rows with
// avatars, quiet outlined actions, and a footer with the connection state
// and the build.
test.describe("Rooms rail", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("brand, labels, the open room, unread pills, actions and footer", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    const rail = page.getByTestId("rooms-rail");

    const brand = rail.getByTestId("rail-brand");
    await expect(brand).toContainText("River");
    expect((await brand.locator("img").boundingBox())!.width).toBeCloseTo(30, 0);

    // Still a heading, now in sentence case and not shouted.
    const heading = rail.getByRole("heading", { name: "Rooms", exact: true });
    await expect(heading).toBeVisible();
    expect(await heading.evaluate((el) => getComputedStyle(el).textTransform)).toBe("none");

    const [link, text, accent, soft] = await Promise.all([
      resolveColor(page, "var(--color-link)"),
      resolveColor(page, "var(--color-text)"),
      resolveColor(page, "var(--color-accent)"),
      resolveColor(page, "var(--color-accent-soft)"),
    ]);
    const open = page.getByTestId("room-list").getByRole("button", { name: "Team Chat Room" });
    const s = await open.evaluate((el) => ({
      color: getComputedStyle(el).color,
      weight: Number(getComputedStyle(el).fontWeight),
      bg: getComputedStyle(el).backgroundColor,
    }));
    expect(await resolveColor(page, s.color), "the open room is not blue").toEqual(text);
    expect(s.weight).toBeGreaterThanOrEqual(600);
    expect((await resolveColor(page, s.bg))[3], "the open room is raised").toBeGreaterThan(0);

    // Example data seeds unread messages in another room.
    const badge = page.getByTestId("room-unread-badge").first();
    await expect(badge).toBeVisible();
    const b = await badge.evaluate((el) => ({ bg: getComputedStyle(el).backgroundColor, color: getComputedStyle(el).color }));
    expect(await resolveColor(page, b.bg), "the pill is the soft accent, not a solid fill").toEqual(soft);
    expect(await resolveColor(page, b.bg)).not.toEqual(accent);
    expect(await resolveColor(page, b.color)).toEqual(link);

    for (const action of [rail.getByTestId("join-with-code-button"), rail.getByTestId("import-id-button")]) {
      await expect(action).toBeVisible();
      const a = await action.evaluate((el) => ({ bg: getComputedStyle(el).backgroundColor, border: getComputedStyle(el).borderTopWidth }));
      expect((await resolveColor(page, a.bg))[3], "outlined, not filled").toBe(0);
      expect(parseFloat(a.border)).toBeGreaterThanOrEqual(1);
    }
    await expect(rail.getByTestId("join-with-code-button")).toHaveText("Enter invite code");
    await expect(rail.getByTestId("import-id-button")).toHaveText("Import ID");

    const footer = rail.getByTestId("rail-footer");
    await expect(footer.getByTestId("connection-status-indicator")).toBeVisible();
    await expect(footer).toContainText("Built: ");
  });

  test("a DM row carries the peer's 22px avatar", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(page, "appendDmsForPeer", 0, 2);
    const avatar = page.getByTestId("dm-rail-avatar").first();
    await expect(avatar).toBeVisible({ timeout: 5_000 });
    await expect(avatar).toHaveText("DT");
    await expect(avatar).toHaveAttribute("aria-hidden", "true");
    expect((await avatar.boundingBox())!.width).toBeCloseTo(22, 0);
    await expect(page.getByRole("heading", { name: "Direct messages", exact: true })).toBeVisible();
  });
});
