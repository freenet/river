//! Platform-conditional `WebApi` type.
//!
//! The rest of the synchronizer stores its node handle as
//! `GlobalSignal<Option<WebApi>>` and calls `.send(req).await` on it.
//! `freenet-stdlib`'s `WebApi` already does the right thing on wasm
//! (the browser impl from `client_api/browser.rs`). On native we can't
//! reuse stdlib's `WebApi` directly because both `send` and `recv`
//! take `&mut self` — that means a single value can't be shared between
//! the "send requests" side (the rest of the synchronizer) and the
//! "drain responses" side (the reader task in `connection_manager`).
//!
//! On native we therefore expose a thin wrapper struct, also named
//! `WebApi`, whose `send` shape is identical to stdlib's. Internally it
//! just hands `ClientRequest`s off to an mpsc channel; the owner task
//! (`ConnectionManager`'s reader/writer loop) holds the real stdlib
//! `WebApi` and translates between the channel and the WebSocket.

#[cfg(target_arch = "wasm32")]
pub use freenet_stdlib::client_api::WebApi;

#[cfg(not(target_arch = "wasm32"))]
pub use native::WebApi;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use freenet_stdlib::client_api::{ClientRequest, Error};
    use tokio::sync::mpsc;

    /// Native `WebApi` handle — wraps the request side of an mpsc channel
    /// whose receiver is owned by the connection-manager task. `send`
    /// mirrors `freenet_stdlib::client_api::WebApi::send`'s signature so
    /// existing call sites (`chat_delegate`, the room/response handlers)
    /// don't need a target cfg.
    pub struct WebApi {
        tx: mpsc::Sender<ClientRequest<'static>>,
    }

    impl WebApi {
        pub(crate) fn new(tx: mpsc::Sender<ClientRequest<'static>>) -> Self {
            Self { tx }
        }

        pub async fn send(&mut self, request: ClientRequest<'static>) -> Result<(), Error> {
            self.tx
                .send(request)
                .await
                .map_err(|_| Error::ChannelClosed)
        }
    }
}
