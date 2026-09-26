import { Page } from "@playwright/test";

// The window.__riverTest hooks the suite calls; ui/src/test_hooks.rs defines them. Later PRs add theirs here.
export type RoomsLoadState = "loading" | "migrating" | "failed" | "loaded";

export type RiverTestHooks = {
  appendMessage(text: string): void;
  insertMessageBeforeLast(text: string): void;
  appendMessages(count: number): void;
  setRoomsLoadState(state: RoomsLoadState): void;
};

// One hook per round trip; timing-sensitive sequences call window.__riverTest in their own evaluate.
export async function callRiverTest<K extends keyof RiverTestHooks>(
  page: Page,
  name: K,
  ...args: Parameters<RiverTestHooks[K]>
): Promise<ReturnType<RiverTestHooks[K]>> {
  return page.evaluate(
    ({ name, args }) => {
      const hooks = (window as { __riverTest?: Record<string, unknown> }).__riverTest;
      const hook = hooks?.[name];
      if (typeof hook !== "function") {
        throw new Error(`window.__riverTest.${name} is not available in this build`);
      }
      return hook.apply(hooks, args);
    },
    { name: name as string, args: args as unknown[] }
  ) as Promise<ReturnType<RiverTestHooks[K]>>;
}
