import { expect, Locator } from "@playwright/test";

// Where the open reaction picker sits against the footer React it was opened
// from. Placement cases live in reaction-picker.spec.ts; message-rows.spec.ts
// keeps two older ones that read the same geometry.

type Box = { left: number; right: number; top: number; bottom: number };

/// The open picker, `row`'s React, the viewport, and whether each emoji is
/// inside the viewport and is the element a press at its centre lands on.
/// Read with no press in between: a press elsewhere would close the picker.
export async function measurePicker(row: Locator) {
  await expect(row.page().getByTestId("emoji-picker")).toBeVisible();
  return row.evaluate((el) => {
    const box = (e: Element): Box => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    const p = document.querySelector('[data-testid="emoji-picker"]')!;
    const react = el.querySelector('[data-testid="message-react-button"]')!;
    const buttons = Array.from(p.querySelectorAll("button")).map((b) => {
      const r = b.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {
        emoji: (b.textContent || "").trim(),
        inside: r.left >= -0.5 && r.top >= -0.5 && r.right <= innerWidth + 0.5 && r.bottom <= innerHeight + 0.5,
        hit: !!at && (at === b || b.contains(at)),
      };
    });
    return { picker: box(p), react: box(react), vw: innerWidth, vh: innerHeight, buttons };
  });
}
export type PickerMeasure = Awaited<ReturnType<typeof measurePicker>>;

/// The gap between the picker and its React.
export const PICKER_GAP = 4;

/// Where the picker sits against its React: below or above it (4px apart),
/// and its right edge on React's right edge or its left edge on React's left.
export function placement(m: PickerMeasure) {
  const { picker: p, react: r } = m;
  const near = (a: number, b: number) => Math.abs(a - b) <= 1;
  return {
    block: near(p.top, r.bottom + PICKER_GAP) ? "below" : near(p.bottom, r.top - PICKER_GAP) ? "above" : `off (${p.top}..${p.bottom} vs ${r.top}..${r.bottom})`,
    inline: near(p.right, r.right) ? "end" : near(p.left, r.left) ? "start" : `off (${p.left}..${p.right} vs ${r.left}..${r.right})`,
  };
}

/// Hung from its React on both axes.
export function expectAtReact(m: PickerMeasure, what = "placed at React") {
  const at = placement(m);
  expect(at.block, what).not.toContain("off");
  expect(at.inline, what).not.toContain("off");
}

export function expectEveryEmojiUsable(m: PickerMeasure) {
  expect(m.buttons.length, "the picker has its emoji").toBe(16);
  const outside = m.buttons.find((b) => !b.inside);
  expect(outside, `${outside?.emoji} is outside the viewport`).toBeUndefined();
  const covered = m.buttons.find((b) => !b.hit);
  expect(covered, `${covered?.emoji} is not what a press at its centre lands on`).toBeUndefined();
}

/// The picker never covers the React it hangs from.
export function expectInvokerUncovered(m: PickerMeasure) {
  const { picker: p, react: r } = m;
  const overlaps = p.left < r.right && p.right > r.left && p.top < r.bottom && p.bottom > r.top;
  expect(overlaps, "the picker covers its React").toBe(false);
}
