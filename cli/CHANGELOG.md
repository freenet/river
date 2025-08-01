# Changelog

All notable changes to riverctl will be documented in this file.

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