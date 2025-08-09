# Changelog

All notable changes to riverctl will be documented in this file.

## [0.1.8] - 2025-08-09

### Fixed
- Publish membership delta on invite accept so other members see invitee
- Add INFO logs for GET/SUBSCRIBE/UPDATE during accept
- Reduce GET/SUBSCRIBE timeouts (2s/1s) to fail fast

## [0.1.7] - 2025-08-01

### Fixed
- Fixed architectural issue with GET operations using `subscribe: true`
  - GET operations now use `subscribe: false` followed by separate SUBSCRIBE operations
  - This fixes compatibility with Freenet's current architecture
  - Both `get_room()` and `accept_invitation()` methods updated
- This fix enables multi-user messaging to work properly

### Technical Details
- GET with subscribe:true requires performing sub-operations from within the main operation and waiting for them to complete, which was never implemented in Freenet
- The fix separates GET and SUBSCRIBE into distinct operations, matching how the River web UI already works

## [0.1.6] - 2025-08-01

### Fixed
- Fixed critical bug where invited users could not send messages after accepting invitations (#28)
  - Room state is now properly initialized when accepting invitations
  - Invited users are correctly added to the members list
  - Member info with nickname is properly created
- Added validation to ensure room state initialization is correct

### Added
- Comprehensive unit tests for invitation flow
- Integration test script for multi-user scenarios

## [0.1.5] - Previous releases...