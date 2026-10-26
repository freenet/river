import { Page } from "@playwright/test";

export type RoomsLoadState = "loading" | "migrating" | "failed" | "loaded";

// Mirrors the window.__riverTest hooks ui/src/test_hooks.rs installs.
type RiverTestHooks = {
  appendMessage(text: string): void;
  insertMessageBeforeLast(text: string): void;
  appendMessages(count: number): void;
  // `count` join events from Test Sender A and B, alternating (two fold into one notice).
  appendJoinEvents(count: number): void;
  removeMessages(containing: string): void;
  // Each emoji as a reaction on the newest message, all held by one test author.
  addReactionsToNewest(emojis: string[]): void;
  // A signed reaction from a test member to the newest message; public rooms only.
  memberReactsToNewest(emoji: string): void;
  // The current room's avatar salt: 32 bytes of `fill`, or none (neutral avatars).
  setAvatarSalt(fill: number | null): void;
  // The seed is read (32 bytes of `fill`) but no salt is stored for the current room.
  setSeedReadUnstored(fill: number): void;
  makeRoomPrivateWithoutSecret(): void;
  forgeNextSendSignature(): void;
  setRoomsLoadState(state: RoomsLoadState): void;
  // Same, without clearing ROOMS: the rail's note under a partly restored list.
  // "migrating" also holds a load worker open until the next call.
  setRoomsLoadStateKeepingRooms(state: RoomsLoadState): void;
  // A journaled walk that may still bring rooms back starts (true) or ends (false).
  setWalkRestoringRooms(on: boolean): void;
  // The held load worker settles; the load state is left as it is.
  releaseHeldLoadWorker(): void;
  switchRoom(name: string): void;
  // Peer 0 and peer 1 are two fixed test identities, each with its own thread. Others throw.
  appendDmsForPeer(peerIndex: 0 | 1, count: number): void;
  appendDmsAcrossDaysForPeer(peerIndex: 0 | 1, count: number): void;
  deliverDmForPeer(peerIndex: 0 | 1, text: string): void;
  // Admits the peer as a member with no DMs, for an empty thread.
  admitDmPeer(peerIndex: 0 | 1): void;
  // An own DM to the peer through the real send path; resolves to the outcome, "Sent" on success.
  sendDmToPeer(peerIndex: 0 | 1, text: string): Promise<string>;
  // How long a mouse click on the room description waits before toggling.
  setDescriptionClickWaitMs(ms: number): void;
  // Message group renders since the last call (the count restarts at 0).
  takeMessageGroupRenders(): number;
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
