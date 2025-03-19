# Chat Delegate

## Overview

The Chat Delegate is a WebAssembly module that provides a key-value storage mechanism for Freenet applications, similar to a browser's localStorage or cookies. It allows applications to:

- Store data (key-value pairs)
- Retrieve data by key
- Delete data by key
- List all stored keys

Each application gets its own isolated storage namespace, ensuring that applications cannot access each other's data.

## Message Protocol

The Chat Delegate uses a request-response protocol defined in `river_common::chat_delegate`:

### Request Messages

```rust
pub enum ChatDelegateRequestMsg {
    StoreRequest {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    GetRequest {
        key : Vec<u8>,
    },
    DeleteRequest {
        key : Vec<u8>,
    },
    ListRequest,
}
```

### Response Messages

```rust
pub enum ChatDelegateResponseMsg {
    GetResponse {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    ListResponse {
        keys: Vec<Vec<u8>>,
    },
    StoreResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
    DeleteResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
}
```

All messages must be serialized using the CBOR format via the `ciborium` crate.

## Implementation Details

### Asynchronous Nature

The delegate system in Freenet is inherently asynchronous, which makes the implementation somewhat complex. When a request comes in:

1. The delegate processes the request
2. It may need to interact with the secret storage system
3. The response from the secret storage comes back as a separate message
4. The delegate must maintain context between these asynchronous steps

This asynchronous flow is particularly evident in operations that modify the key index (store, delete), which require multiple steps:
1. Respond to the client immediately (optimistically)
2. Perform the actual storage operation
3. Retrieve the current key index
4. Update the key index with the new/removed key
5. Store the updated key index

### Key Indexing

To support the `ListRequest` operation, the delegate maintains a special "key index" for each application. This index is stored as a secret with a special suffix (`::key_index`). When keys are stored or deleted, this index is automatically updated.

### Future Improvements

In future versions of Freenet, the delegate mechanism will be simplified to make asynchronous operations less complex, potentially with:
- Direct support for key-value storage
- Built-in indexing capabilities
- Simplified context management

## Usage Example

```rust
// Serialize a request to store data
let request = ChatDelegateRequestMsg::StoreRequest {
    key: b"user_preferences".to_vec(),
    value: serde_json::to_vec(&preferences).unwrap(),
};
let mut request_bytes = Vec::new();
ciborium::ser::into_writer(&request, &mut request_bytes).unwrap();

// Send the request to the delegate
// ... application-specific code to send the message ...

// When receiving a response
let response: ChatDelegateResponseMsg = ciborium::from_reader(&response_bytes[..]).unwrap();
match response {
    ChatDelegateResponseMsg::StoreResponse { result, .. } => {
        if let Err(e) = result {
            println!("Failed to store data: {}", e);
        }
    },
    // Handle other response types
    _ => {}
}
```
