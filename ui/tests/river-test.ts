import { Page } from "@playwright/test";

export type RoomsLoadState = "loading" | "migrating" | "failed" | "loaded";

// Mirrors the window.__riverTest hooks ui/src/test_hooks.rs installs.
type RiverTestHooks = {
  /// Real controller callbacks, installed once the history has mounted.
  takeInPendingHistoryScroll(): void;
  restoreHistoryPosition(): void;
  appendMessage(text: string): void;
  insertMessageBeforeLast(text: string): void;
  appendMessages(count: number): void;
  /// Appends a join event, which renders as an event-summary row.
  appendJoinEvent(): void;
  setRoomsLoadState(state: RoomsLoadState): void;
  switchRoom(name: string): void;
  /// Removes the messages whose rows have these DOM ids (`msg-...`) in one
  /// state change. Resolves with the ids that matched no message.
  removeMessages(domIds: string[]): Promise<string[]>;
  /// Renders the current room's history with no rows (`true`), through the
  /// real memo and render, or with its unchanged rows again (`false`). Resolves
  /// once the deferred state change has run, not once the DOM shows it. A room
  /// switch ends it.
  setHistoryEmpty(on: boolean): Promise<void>;
};

declare global {
  interface Window {
    /// Installed by `App` in example-data, no-sync builds (ui/src/test_hooks.rs);
    /// absent before the app starts and in every other build.
    __riverTest?: RiverTestHooks;
  }
}

// One hook per round trip.
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
