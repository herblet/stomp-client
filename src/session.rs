use std::fmt::Debug;

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use stomp_parser::server::MessageFrame;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    error::StompClientError,
    event_loop::{DestinationId, SessionEvent},
};

pub struct StompClientSession {
    event_sender: UnboundedSender<SessionEvent>,
}

impl Debug for StompClientSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StompClientSession")
    }
}

type IncomingMessageStream = UnboundedReceiverStream<MessageFrame>;

impl StompClientSession {
    pub fn new(event_sender: UnboundedSender<SessionEvent>) -> StompClientSession {
        StompClientSession { event_sender }
    }

    pub fn subscribe(
        &self,
        destination: String,
    ) -> Result<IncomingMessageStream, StompClientError> {
        let (sender, receiver) = unbounded_channel::<MessageFrame>();

        self.send_event(SessionEvent::Subscribe(
            DestinationId(destination.into()),
            sender,
        ))
        .map(|()| UnboundedReceiverStream::new(receiver))
    }

    pub fn send(&self, destination: String, message: Vec<u8>) -> Result<(), StompClientError> {
        self.send_event(SessionEvent::Send(DestinationId(destination), message))
    }

    fn send_event(&self, event: SessionEvent) -> Result<(), StompClientError> {
        self.event_sender
            .send(event)
            .map_err(|_| StompClientError::new("Unable to create subscription"))
            .map(|_| ())
    }
}

/// The fact that the sender is dropped does not suffice, because clones may exist. So the event-loop
/// needs to be closed explicitly
impl Drop for StompClientSession {
    fn drop(&mut self) {
        if let Err(_) = self.send_event(SessionEvent::Close) {
            log::error!("Event loop stopped before session dropped");
        };
    }
}
