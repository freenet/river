import { test, expect } from "@playwright/test";
import { resolveColor, selectRoom, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";
import { DM_PEER, dm, openThread } from "./dm-thread";

// A DM thread reads like a room: grouped rows with an avatar and a name +
// time header per group, tight continuation rows and a tint on your own
// rows, not left and right bubbles.
test.describe("DM thread rows", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("groups by sender, with one avatar and header per group, and your rows tinted", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(page, "appendDmsForPeer", DM_PEER.index, 3);
    await openThread(page, DM_PEER, "dm history 02");
    expect(await callRiverTest(page, "sendDmToPeer", DM_PEER.index, "my reply in rows")).toBe("Sent");
    await expect(dm(page, "my reply in rows")).toHaveCount(1);

    const rows = page.getByTestId("dm-row");
    const m = await rows.evaluateAll((els) =>
      els.map((el) => ({
        self: el.getAttribute("data-message-self"),
        avatar: !!el.querySelector(".msg-avatar"),
        header: el.querySelector('[data-testid="dm-group-header"]')?.textContent ?? null,
        left: el.getBoundingClientRect().left,
        bodyLeft: el.querySelector('[data-testid="dm-body"]')?.getBoundingClientRect().left ?? null,
        bg: getComputedStyle(el).backgroundColor,
      })),
    );
    const inbound = m.filter((r) => r.self === "false");
    const own = m.filter((r) => r.self === "true");
    expect(inbound.length).toBeGreaterThanOrEqual(3);
    expect(own).toHaveLength(1);
    // One avatar and one header for the peer's run, on its first row.
    expect(inbound.map((r) => r.avatar)).toEqual([true, ...Array(inbound.length - 1).fill(false)]);
    expect(inbound[0].header).toContain(DM_PEER.name);
    expect(own[0].avatar).toBe(true);
    // Both sides share the content edge: no right-aligned bubbles.
    const lefts = m.map((r) => r.bodyLeft!).filter((x) => x !== null);
    expect(Math.max(...lefts) - Math.min(...lefts)).toBeLessThanOrEqual(0.5);
    // Your row is tinted, the peer's are not.
    expect(await resolveColor(page, own[0].bg)).toEqual(await resolveColor(page, "var(--color-row-self)"));
    expect((await resolveColor(page, inbound[1].bg))[3]).toBe(0);
  });
});
