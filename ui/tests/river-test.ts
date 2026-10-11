import { Page } from "@playwright/test";

export type RoomsLoadState = "loading" | "migrating" | "failed" | "loaded";

// Mirrors the window.__riverTest hooks ui/src/test_hooks.rs installs.
type RiverTestHooks = {
  appendMessage(text: string): void;
  insertMessageBeforeLast(text: string): void;
  appendMessages(count: number): void;
  removeMessages(containing: string): void;
  makeRoomPrivateWithoutSecret(): void;
  setRoomsLoadState(state: RoomsLoadState): void;
  switchRoom(name: string): void;
  // Peer 0 and peer 1 are two fixed test identities, each with its own thread. Others throw.
  appendDmsForPeer(peerIndex: 0 | 1, count: number): void;
  deliverDmForPeer(peerIndex: 0 | 1, text: string): void;
  // Admits the peer as a member with no DMs, for an empty thread.
  admitDmPeer(peerIndex: 0 | 1): void;
};

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
