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
  // Invite-via-DM sends wait before signing until releaseInviteSends.
  holdInviteSends(): void;
  releaseInviteSends(): void;
  // How long the invite picker waits for a send before closing itself.
  setInvitePickerWatchdogMs(ms: number): void;
  // The app toast, with an action button when actionLabel is given.
  showToast(message: string, actionLabel: string | null): void;
  // Runs of a showToast action since the last call (the count restarts at 0).
  takeToastActionRuns(): number;
  // The generation-update banner: "manual" (Reload), "countdown" (Reload, Not now) or none.
  showUpdateNotice(kind: "manual" | "countdown" | null): void;
  // The connection pill's status, through the app's deferred status setter.
  // "error" is a closed connection; "error:<message>" a Disconnected report with that text.
  setSyncStatus(status: SyncStatus): void;
  // A connection error report with one of the report labels.
  setSyncErrorReport(label: ReportLabel, summary: string, detail: string): void;
  // The current room as a placeholder awaiting its initial sync that ended in this error.
  showRoomSyncError(label: ReportLabel, summary: string, detail: string): void;
  // The same, with the room banner's first read of that error failing as a contended read does.
  showRoomSyncErrorFailingBannerRead(label: ReportLabel, summary: string, detail: string): void;
  // Room banner sync-status reads that failed since the last call (the count restarts at 0).
  takeBannerReadFailures(): number;
  // Room banner reads that found the room's sync error since the last call (restarts at 0).
  takeBannerErrorReads(): number;
};

export type SyncStatus = "connected" | "connecting" | "disconnected" | "error" | `error:${string}`;
type ReportLabel =
  | "Disconnected"
  | "Invalid operation"
  | "Unexpected message"
  | "Refreshing..."
  | "Room not found"
  | "Room is full";

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
