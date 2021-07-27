use futures::channel::mpsc::UnboundedSender;

use crate::event_loop::SessionEvent;

pub struct StompClientSession {
    event_sender: UnboundedSender<SessionEvent>,
}

impl StompClientSession {
    pub fn new(event_sender: UnboundedSender<SessionEvent>) -> StompClientSession {
        StompClientSession { event_sender }
    }
}
