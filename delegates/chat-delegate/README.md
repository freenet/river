# ChatDelegate Overview

The ChatDelegate is a key-value storage system for chat applications in Freenet. It provides origin-based data partitioning similar to a web browser's localStorage API, ensuring data isolation between different applications. Let me walk through how it handles different types of messages and the overall flow of operations.

## Overview

The ChatDelegate provides four main operations:
1. **Store** - Save data with a specific key
2. **Get** - Retrieve data for a key
3. **Delete** - Remove data for a key
4. **List** - Get all available keys

Each operation involves a multi-step process due to the asynchronous nature of the delegate system.

## Origin-Based Data Partitioning

A key security feature of the ChatDelegate is that all data is partitioned by "origin contract" - the contract key from which the user interface was downloaded. This works similarly to a web browser's localStorage, where data is partitioned by the hostname/port of the website:

1. Each piece of data is stored with a composite key that includes the origin contract ID
2. Applications can only access data that was stored by the same origin
3. This prevents different applications from accessing each other's data
4. The partitioning happens automatically and is transparent to the application

This design ensures that even if multiple chat applications use the same delegate, their data remains isolated and secure from each other.

## Message Flow Architecture

The delegate uses a state machine pattern where:
1. Initial application messages trigger operations
2. State is stored in a context object between operations
3. Responses from the storage system trigger follow-up actions

## Processing Inbound Messages

The entry point is the `process` method in `lib.rs`, which handles four types of messages:

```rust
match message {
    InboundDelegateMsg::ApplicationMessage(app_msg) => {
        // Handle client requests (Store/Get/Delete/List)
        handle_application_message(app_msg, &origin)
    }
    InboundDelegateMsg::GetSecretResponse(response) => {
        // Handle responses from the storage system
        handle_get_secret_response(response)
    }
    InboundDelegateMsg::UserResponse(_) => {
        // Not used in this delegate
        Ok(vec![])
    }
    InboundDelegateMsg::GetSecretRequest(_) => {
        // Not handled directly
        Err(DelegateError::Other("unexpected message type: get secret request".into()))
    }
}
```

## Key Concepts

Before diving into specific flows, let's understand some key concepts:

1. **Origin**: The contract ID that identifies the application
2. **ChatDelegateKey**: A wrapper around a byte vector that represents a key
3. **KeyIndex**: A list of all keys for a specific origin
4. **PendingOperation**: State stored in the context to track ongoing operations

## 1. Store Operation Flow

When a client wants to store data:

1. **Client sends a StoreRequest**:
   ```rust
   ChatDelegateRequestMsg::StoreRequest { key, value }
   ```

2. **Delegate processes the request** (`handle_store_request`):
    - Creates a unique storage key by combining the origin and client key
    - Stores the operation in context for later processing
    - Immediately sends back a success response to the client
    - Sends a request to store the actual value
    - Requests the current key index to update it

3. **Delegate receives the key index** (`handle_key_index_response`):
    - Adds the new key to the index if it doesn't exist
    - Updates the index in storage
    - The operation is complete

## 2. Get Operation Flow

When a client wants to retrieve data:

1. **Client sends a GetRequest**:
   ```rust
   ChatDelegateRequestMsg::GetRequest { key }
   ```

2. **Delegate processes the request** (`handle_get_request`):
    - Creates the unique storage key
    - Stores the operation in context
    - Sends a request to get the value from storage

3. **Delegate receives the value** (`handle_regular_get_response`):
    - Retrieves the pending operation from context
    - Sends the value back to the client
    - The operation is complete

## 3. Delete Operation Flow

When a client wants to delete data:

1. **Client sends a DeleteRequest**:
   ```rust
   ChatDelegateRequestMsg::DeleteRequest { key }
   ```

2. **Delegate processes the request** (`handle_delete_request`):
    - Creates the unique storage key
    - Stores the operation in context
    - Immediately sends back a success response to the client
    - Sends a request to delete the value (by setting it to None)
    - Requests the current key index to update it

3. **Delegate receives the key index** (`handle_key_index_response`):
    - Removes the key from the index
    - Updates the index in storage
    - The operation is complete

## 4. List Operation Flow

When a client wants to list all keys:

1. **Client sends a ListRequest**:
   ```rust
   ChatDelegateRequestMsg::ListRequest
   ```

2. **Delegate processes the request** (`handle_list_request`):
    - Stores the operation in context
    - Requests the current key index

3. **Delegate receives the key index** (`handle_key_index_response`):
    - Sends the list of keys back to the client
    - The operation is complete

## Key Management

A critical aspect of the delegate is how it manages keys:

1. **Origin-Based Key Namespacing**: Each key is prefixed with the origin contract ID to enforce data partitioning:
   ```rust
   pub(crate) fn create_origin_key(origin: &Origin, key: &ChatDelegateKey) -> SecretsId {
       SecretsId::new(
           format!("{}{}{}", origin.to_b58(), ORIGIN_KEY_SEPARATOR, 
                  String::from_utf8_lossy(key.as_bytes()).to_string()).into_bytes()
       )
   }
   ```
   
   This ensures that:
   - Data from different origins is completely isolated
   - Applications can only access their own data
   - Key collisions between different applications are impossible

2. **Origin-Specific Key Index**: Each origin has its own separate key index:
   ```rust
   pub(crate) fn create_index_key(origin: &Origin) -> SecretsId {
       SecretsId::new(format!(
           "{}{}{}",
           origin.to_b58(),
           ORIGIN_KEY_SEPARATOR,
           KEY_INDEX_SUFFIX
       ).into_bytes())
   }
   ```
   
   This means:
   - Each application has its own isolated list of keys
   - The ListRequest operation only returns keys for the calling application's origin
   - Applications cannot enumerate keys from other origins

## Context Management

The delegate uses a context object to maintain state between operations:

```rust
pub(super) struct ChatDelegateContext {
    pub(super) pending_ops: HashMap<SecretIdKey, PendingOperation>,
}
```

This context is serialized and passed along with requests, then deserialized when responses are received.

## Tying It All Together

The overall flow for any operation follows this pattern:

1. **Client Request**: Application sends a request to the delegate
2. **Initial Processing**: Delegate creates necessary storage keys and stores state in context
3. **Storage Operations**: Delegate interacts with the storage system
4. **Response Handling**: Delegate processes storage responses and updates state
5. **Client Response**: Delegate sends final response back to the application

This asynchronous, multi-step approach allows the delegate to maintain consistency while providing a simple interface to client applications.

## Example: Complete Store Flow

Let's trace a complete store operation with origin partitioning:

1. Client sends `StoreRequest { key: "user123", value: [profile data] }`
2. Delegate:
    - Identifies the origin contract ID (e.g., `abc123`) from which the request came
    - Creates storage key: `abc123:user123` (prefixing the client key with origin)
    - Creates index key: `abc123::key_index` (origin-specific index)
    - Stores pending operation in context
    - Sends success response to client
    - Sends request to store value at `abc123:user123`
    - Sends request to get current index at `abc123::key_index`
3. Delegate receives index (or empty if first key)
4. Delegate:
    - Adds "user123" to index if not present
    - Updates index in storage
    - Operation complete

If a different application with origin `xyz789` tries to access this data:
- It would use key `xyz789:user123` which is different from `abc123:user123`
- It would not find the data stored by the first application
- It would have its own separate key index at `xyz789::key_index`

This architecture ensures both data consistency and security through origin-based isolation, while providing a responsive experience for client applications.
