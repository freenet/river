# River Chat Application Overview

River is a decentralized chat application built on Freenet with the following key characteristics:

## Architecture

- **Frontend:** Dioxus-based WebAssembly UI running in the browser
- **Backend:** Freenet network for decentralized storage and communication
- **Deployment:** The application is packaged, signed, and published as a Freenet contract
- **Communication:** Uses WebSocket API to interact with the local Freenet node

## Key Components

1. **FreenetApiSynchronizer** – Manages WebSocket communication with Freenet
2. **Room State Management** – Implements a commutative monoid pattern for order-agnostic state
   updates
3. **Invitation System** – Allows users to invite others to chat rooms
4. **Cryptographic Security** – Uses ed25519 for signatures and authentication

## Implementation Details

- Uses a comprehensive logging system for debugging WebSocket interactions
- State updates are designed to be commutative (order-independent)
- Handles both full state updates and delta updates
- Implements proper error handling and status reporting

## Deployment Process

1. **Build the UI:** `cargo make build-ui`
2. **Compress the webapp:** `cargo make compress-webapp`
3. **Sign the webapp:** `cargo make sign-webapp`
4. **Publish to Freenet:** `cargo make publish-river`

## Testing Challenges

- Testing requires the full deployment pipeline
- Cannot easily test components in isolation
- Need to publish to Freenet and access via the Freenet web interface

## Current Status

- Core functionality is implemented
- WebSocket API integration is ready for testing
- Invitation system is implemented but untested
- Limited runway to prove the concept works
