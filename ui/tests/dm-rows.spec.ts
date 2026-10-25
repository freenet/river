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
        startsGroup: el.parentElement!.firstElementChild === el,
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
    // One avatar and one header per group, on its first row. The peer's run
    // is one group, or two when it straddles midnight as the test runs (a
    // group never spans a day break).
    expect(m.map((r) => r.avatar), "an avatar exactly where a group starts").toEqual(m.map((r) => r.startsGroup));
    const inboundGroups = inbound.filter((r) => r.startsGroup).length;
    expect(inboundGroups).toBe(1 + (inboundGroups > 1 ? (await page.getByTestId("dm-date-separator").count()) - 1 : 0));
    expect(inbound[0].header).toContain(DM_PEER.name);
    expect(own[0].avatar).toBe(true);
    // Both sides share the content edge: no right-aligned bubbles.
    const lefts = m.map((r) => r.bodyLeft!).filter((x) => x !== null);
    expect(Math.max(...lefts) - Math.min(...lefts)).toBeLessThanOrEqual(0.5);
    // Your row is tinted, the peer's are not.
    expect(await resolveColor(page, own[0].bg)).toEqual(await resolveColor(page, "var(--color-row-self)"));
    expect((await resolveColor(page, inbound[1].bg))[3]).toBe(0);
  });

  test("a thread that spans days has a day break like a room's", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(page, "appendDmsAcrossDaysForPeer", DM_PEER.index, 6);
    await openThread(page, DM_PEER, "dm history 05");
    const seps = page.getByTestId("dm-date-separator");
    // Yesterday's half and today's (a third label only when the run crosses
    // midnight as the test runs).
    await expect.poll(() => seps.count()).toBeGreaterThanOrEqual(2);
    const labels = await seps.allTextContents();
    expect(new Set(labels).size, "one break per day").toBe(labels.length);
    expect(labels[labels.length - 1]).toBe("Today");
    // The break sits between the days: above the first of today's DMs, below
    // the last of the earlier day's.
    const order = await page.evaluate(() => {
      const body = document.querySelector('[data-testid="dm-row"]')!.closest(".msg-group")!.parentElement!;
      return Array.from(body.children).map((el) =>
        el.matches('[data-testid="dm-date-separator"]') ? "day" : (el.textContent!.match(/dm history \d\d/g) ?? []).join(","),
      );
    });
    const firstDay = order.indexOf("day");
    expect(firstDay, "a break above the first group").toBe(0);
    const today = order.lastIndexOf("day");
    expect(order[today + 1]).toContain("dm history 0");
    expect(order.slice(0, today).join(",")).toContain("dm history 02");
    expect(order.slice(today).join(",")).toContain("dm history 05");
    // The same hairline style as the room's separator.
    const style = await seps.last().evaluate((el) => {
      const cs = getComputedStyle(el);
      return { size: cs.fontSize, weight: cs.fontWeight, rule: getComputedStyle(el, "::before").height };
    });
    expect(style).toEqual({ size: "12px", weight: "600", rule: "1px" });
  });
});
