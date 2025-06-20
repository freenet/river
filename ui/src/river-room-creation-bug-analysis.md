# River Room Creation Bug Analysis

## Problem
When creating a room in River, no PUT request is sent to Freenet, preventing the room from being stored in the network.

## Root Cause
The room creation flow in River has a missing step - it doesn't trigger the synchronization process after creating a room locally.

## How Room Creation Works

1. **User clicks "Create Room"** in the UI
2. **CreateRoomModal component** (`create_room_modal.rs`):
   - Calls `ROOMS.with_mut(|rooms| rooms.create_new_room_with_name(...))`
   - This creates the room data locally in memory
   - Updates `CURRENT_ROOM` to the new room

3. **Room synchronization** should happen via:
   - `App` component has a `use_effect` that watches `ROOMS` for changes
   - When ROOMS changes, it sends `ProcessRooms` message to the synchronizer
   - The synchronizer's `process_rooms()` method checks for rooms needing sync
   - Rooms with `RoomSyncStatus::Disconnected` trigger a PUT request

## The Bug
The `use_effect` in `App.rs` that monitors ROOMS changes might not be triggered when using `ROOMS.with_mut()`. This is because:
- `with_mut()` provides mutable access to the inner value
- But it might not trigger Dioxus's reactivity system
- Therefore, the `use_effect` doesn't run, and no `ProcessRooms` message is sent

## Solution Options

1. **Immediate fix**: After creating a room, explicitly send the `ProcessRooms` message:
   ```rust
   // In create_room_modal.rs, after creating the room:
   let synchronizer = SYNCHRONIZER.read();
   let message_sender = synchronizer.get_message_sender();
   message_sender.unbounded_send(SynchronizerMessage::ProcessRooms).ok();
   ```

2. **Better fix**: Ensure ROOMS mutations trigger the effect properly by using the signal's API correctly

3. **Alternative**: Add a direct "sync new room" method that immediately sends the PUT request

## Additional Notes
- The synchronizer and room processing logic appears correct
- WebSocket connections are established properly
- The issue is purely in the triggering mechanism between room creation and synchronization