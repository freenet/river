use std::fmt;
use futures::channel::mpsc::UnboundedSender;
use freenet_stdlib::client_api::ClientRequest;

/// Sender handle for making requests to the Freenet API
#[derive(Clone)]
pub struct FreenetApiSender {
    /// Channel sender for client requests
    pub request_sender: UnboundedSender<ClientRequest<'static>>,
}

impl fmt::Debug for FreenetApiSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FreenetApiSender")
            .field("request_sender", &"<UnboundedSender>")
            .finish()
    }
}
