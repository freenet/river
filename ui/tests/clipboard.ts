import { Page } from "@playwright/test";

/**
 * Capture what the app actually writes to the clipboard.
 *
 * `crate::util::copy_to_clipboard` goes through `document.execCommand('copy')`
 * on a throwaway off-screen textarea (so it works inside the gateway's
 * sandboxed iframe), which is not readable via the Clipboard API. Patching
 * `execCommand` lets us assert the COPIED TEXT rather than just the button's
 * label — a button wired to the wrong field would still say "Copied!".
 */
export async function captureClipboardWrites(page: Page) {
  await page.evaluate(() => {
    const w = window as unknown as { __clip: string[] };
    w.__clip = [];
    const orig = document.execCommand.bind(document);
    document.execCommand = function (cmd: string, ...rest: unknown[]) {
      if (cmd === "copy") {
        const active = document.activeElement as HTMLTextAreaElement | null;
        let copied = active && "value" in active ? active.value : "";
        if (!copied) {
          // `select()` is not guaranteed to focus, so fall back to the
          // helper's signature: an off-screen fixed-position textarea.
          const ta = Array.from(document.querySelectorAll("textarea")).find(
            (t) => t.style.left === "-9999px"
          );
          copied = ta?.value ?? "";
        }
        w.__clip.push(copied);
      }
      return (orig as (c: string, ...r: unknown[]) => boolean)(cmd, ...rest);
    } as typeof document.execCommand;
  });
}

export function clipboardWrites(page: Page) {
  return page.evaluate(() => (window as unknown as { __clip: string[] }).__clip);
}
