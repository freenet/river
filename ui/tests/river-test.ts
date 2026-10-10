import { Page } from "@playwright/test";

export type RoomsLoadState = "loading" | "migrating" | "failed" | "loaded";

// Mirrors the window.__riverTest hooks ui/src/test_hooks.rs installs.
type RiverTestHooks = {
  appendMessage(text: string): void;
  appendMessageAhead(text: string, seconds: number): void;
  insertMessageBeforeLast(text: string): void;
  appendMessages(count: number): void;
  removeMessages(containing: string): void;
  makeRoomPrivateWithoutSecret(): void;
  setRoomsLoadState(state: RoomsLoadState): void;
  switchRoom(name: string): void;
  appendDms(count: number): void;
  deliverDm(text: string): void;
  deliverDmAhead(text: string, seconds: number): void;
  failNextDmRoomRead(): void;
  dmRoomReadFailuresTaken(): number;
  // Peer 0 is the identity appendDms/deliverDm use; peer 1 a second one. Others throw.
  appendDmsForPeer(peerIndex: 0 | 1, count: number): void;
  deliverDmForPeer(peerIndex: 0 | 1, text: string): void;
  // Admits the peer as a member with no DMs, for an empty thread.
  admitDmPeer(peerIndex: 0 | 1): void;
  holdNextDmPlacement(): void;
  heldDmPlacementCount(): number;
  // Throws if no placement is held.
  releaseHeldDmPlacement(): void;
  releasedDmPlacementsRun(): number;
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
