use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    fmt::Debug,
    pin::Pin,
};

use futures::{
    future::ready,
    stream::{empty, once, select_all, BoxStream},
    Future, FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt, TryStreamExt,
};
use stomp_parser::{client::*, headers::*, server::*};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{error::StompClientError, session::StompClientSession, spawn};

pub type BytesStream = BoxStream<'static, Result<Vec<u8>, StompClientError>>;
pub type BytesSink = Pin<Box<dyn Sink<Vec<u8>, Error = StompClientError> + Sync + Send + 'static>>;

pub struct DestinationId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriptionId(pub String);

pub enum SessionEvent {
    Subscribe(DestinationId, UnboundedSender<MessageFrame>),
    Unsubscribe(SubscriptionId),
    Send(DestinationId, Vec<u8>),
    ServerFrame(Result<ServerFrame, StompClientError>),
    ServerHeartBeat,
    Close,
    Disconnected,
}

impl Debug for SessionEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Subscribe(_, _) => "Subscribe",
            Self::Unsubscribe(_) => "Unsubscribe",
            Self::Send(_, _) => "Send",
            Self::ServerFrame(_) => "ServerFrame",
            Self::ServerHeartBeat => "ServerHeartBeat",
            Self::Close => "Close",
            Self::Disconnected => "Disconnected",
        })
    }
}
pub enum SessionState {
    Alive,
    Dead,
}

pub enum MessageToServer {
    ClientFrame(ClientFrame),
    Heartbeat,
}

type EventResult = Result<(SessionState, Option<MessageToServer>), StompClientError>;

type EventResultFuture = Pin<Box<dyn Future<Output = EventResult> + Send + 'static>>;

pub struct SessionEventLoop {
    client_event_sender: UnboundedSender<SessionEvent>,
    subscriptions: HashMap<SubscriptionId, UnboundedSender<MessageFrame>>,
}

async fn perform_handshake(
    host_id: String,
    stream_from_server: &mut BytesStream,
    sink_to_server: &mut BytesSink,
) -> Result<(), StompClientError> {
    sink_to_server.send(connect_frame(host_id)).await?;

    match stream_from_server.next().await {
        None => Err(StompClientError::new(
            "Stream from server ended before handshake",
        )),
        Some(Err(client_err)) => Err(client_err),
        Some(Ok(bytes)) => validate_handshake(Some(bytes)),
    }
}

impl SessionEventLoop {
    fn new(client_event_sender: UnboundedSender<SessionEvent>) -> SessionEventLoop {
        SessionEventLoop {
            client_event_sender,
            subscriptions: HashMap::new(),
        }
    }

    pub async fn start(
        host_id: String,
        mut stream_from_server: BytesStream,
        mut sink_to_server: BytesSink,
    ) -> Result<StompClientSession, StompClientError> {
        perform_handshake(host_id, &mut stream_from_server, &mut &mut sink_to_server).await?;
        // the handshake complete succressfully!

        let stream_from_server = create_server_event_stream(stream_from_server);

        let (client_event_sender, stream_from_client) = unbounded_channel();

        let stream_from_client = UnboundedReceiverStream::new(stream_from_client)
            .chain(once(ready(SessionEvent::Close)));

        let client_heartbeat_stream = empty();

        let all_events = select_all(vec![
            stream_from_client.boxed(),
            stream_from_server.boxed(),
            client_heartbeat_stream.boxed(),
        ])
        .inspect(|item| {
            log::debug!("Event: {:?}", item);
        });

        let event_handler = {
            let mut session = SessionEventLoop::new(client_event_sender.clone());

            // client_session.start_heartbeat_listener(client_heartbeat_stream);

            move |event| session.handle_event(event)
        };

        let response_stream = all_events
            .then(event_handler)
            .inspect_ok(|_| {
                log::debug!("Response");
            })
            .inspect_err(Self::log_error)
            .take_while(Self::not_dead)
            .filter_map(Self::into_opt_ok_of_bytes)
            .forward(sink_to_server)
            .map(|_| ());

        spawn(response_stream);

        Ok(StompClientSession::new(client_event_sender))
    }

    fn into_opt_ok_of_bytes(
        result: Result<(SessionState, Option<MessageToServer>), StompClientError>,
    ) -> impl Future<Output = Option<Result<Vec<u8>, StompClientError>>> {
        ready(
            // Drop the ClientState, already handled
            result
                .map(|(_, opt_frame)| {
                    // serialize the frame
                    opt_frame.map(|either| match either {
                        MessageToServer::ClientFrame(frame) => {
                            frame.try_into().expect("Serialisation failed")
                        }
                        MessageToServer::Heartbeat => b"\n".to_vec(),
                    })
                })
                // drop errors
                .or(Ok(None))
                // cause only Some(Ok(bytes)) values to be passed on
                .transpose(),
        )
    }

    fn log_error(error: &StompClientError) {
        log::error!("SessionEventLoop error: {:?}", error);
    }

    fn not_dead(event_result: &EventResult) -> impl Future<Output = bool> {
        ready(!matches!(event_result, Ok((SessionState::Dead, _))))
    }

    fn subscribe(
        &mut self,
        destination: DestinationId,
        sender: UnboundedSender<MessageFrame>,
    ) -> EventResult {
        let subscription_id = SubscriptionId(self.subscriptions.len().to_string());

        self.start_subscription_end_listener(sender.clone(), subscription_id.clone());

        log::info!(
            "Subscribing to destination '{:?}' with id '{:?}'",
            &destination.0,
            &subscription_id
        );

        let message_to_server = MessageToServer::ClientFrame(ClientFrame::Subscribe(
            SubscribeFrameBuilder::new(destination.0, subscription_id.0.clone()).build(),
        ));

        self.subscriptions.insert(subscription_id, sender);

        Ok((SessionState::Alive, Some(message_to_server)))
    }

    fn start_subscription_end_listener(
        &self,
        sender: UnboundedSender<MessageFrame>,
        subscription: SubscriptionId,
    ) {
        let client_event_sender = self.client_event_sender.clone();
        spawn(async move {
            sender.closed().await;
            if let Err(err) = client_event_sender.send(SessionEvent::Unsubscribe(subscription)) {
                log::error!("Unable to unsubscribe, event-loop dead? Error: {:?}", err);
            }
        });
    }

    fn unsubscribe(&mut self, subscription: SubscriptionId) -> EventResult {
        match self.subscriptions.remove(&subscription) {
            Some(_) => {
                let message_to_server = MessageToServer::ClientFrame(ClientFrame::Unsubscribe(
                    UnsubscribeFrameBuilder::new(subscription.0.clone()).build(),
                ));
                Ok((SessionState::Alive, Some(message_to_server)))
            }
            None => Err(StompClientError::new(format!(
                "Tried to remove missing subscription {:?}",
                subscription,
            ))),
        }
    }

    fn send(&mut self, destination: DestinationId, content: Vec<u8>) -> EventResult {
        let message_to_server = MessageToServer::ClientFrame(ClientFrame::Send(
            SendFrameBuilder::new(destination.0).body(content).build(),
        ));
        Ok((SessionState::Alive, Some(message_to_server)))
    }

    fn handle_server_frame(&mut self, server_frame: ServerFrame) -> EventResultFuture {
        log::info!("Handling server frame.");
        match server_frame {
            ServerFrame::Message(message_frame) => ready(self.forward_message(message_frame))
                .map_ok(|()| (SessionState::Alive, None))
                .boxed(),
            _ => todo!(),
        }
    }

    fn forward_message(&mut self, message_frame: MessageFrame) -> Result<(), StompClientError> {
        self.subscriptions
            .get(&SubscriptionId(
                message_frame.subscription.value().to_owned(),
            ))
            .ok_or_else(|| {
                StompClientError::new(format!(
                    "Received message for unknown subscription {}",
                    message_frame.subscription.value()
                ))
            })
            .and_then(|sender_for_sub| {
                sender_for_sub.send(message_frame).map_err(|err| {
                    StompClientError::new(format!("error sending message to handler: {}", err))
                })
            })
    }

    fn handle_event(&mut self, event: SessionEvent) -> EventResultFuture {
        match event {
            SessionEvent::Subscribe(destination, message_sender) => {
                ready(self.subscribe(destination, message_sender)).boxed()
            }

            SessionEvent::Unsubscribe(subscription_id) => {
                ready(self.unsubscribe(subscription_id)).boxed()
            }

            SessionEvent::Send(destination, content) => {
                ready(self.send(destination, content)).boxed()
            }

            SessionEvent::ServerFrame(Ok(server_frame)) => self.handle_server_frame(server_frame),

            SessionEvent::ServerFrame(Err(error)) => {
                log::error!("Error processing message from server: {:?}", error);
                ready(Err(error)).boxed()
            }

            SessionEvent::Close => {
                log::info!("Session closed by client.");
                ready(Ok((SessionState::Dead, None))).boxed()
            }

            SessionEvent::Disconnected => {
                log::error!("Connection dropped by server.");
                ready(Ok((SessionState::Dead, None))).boxed()
            }

            ev => {
                log::info!("Unknown event received: {:?}", ev);
                todo!()
            }
        }
    }
}

fn create_server_event_stream(raw_stream: BytesStream) -> impl Stream<Item = SessionEvent> {
    // Closes this session; will be chained to client stream to run after that ends
    let close_stream = once(async { SessionEvent::Disconnected }).boxed();

    raw_stream
        .and_then(|bytes| parse_server_message(bytes).boxed())
        .map(|opt_frame| {
            opt_frame
                .transpose()
                .map(SessionEvent::ServerFrame)
                .unwrap_or(SessionEvent::ServerHeartBeat)
        })
        .chain(close_stream)
}

async fn parse_server_message(bytes: Vec<u8>) -> Result<Option<ServerFrame>, StompClientError> {
    if is_heartbeat(&*bytes) {
        Ok(None)
    } else {
        Some(ServerFrame::try_from(bytes).map_err(|err| err.into())).transpose()
    }
}

fn is_heartbeat(bytes: &[u8]) -> bool {
    matches!(bytes, b"\n" | b"\r\n")
}

fn validate_handshake(response: Option<Vec<u8>>) -> Result<(), StompClientError> {
    match response {
        None => Err(StompClientError::new("Expected Some bytes")),
        Some(bytes) => match ServerFrame::try_from(bytes) {
            Ok(ServerFrame::Connected(connected_frame)) => {
                if let StompVersion::V1_2 = connected_frame.version.value() {
                    // Start the event loop, and return the session
                    //remainder.for_each(|_| async {});
                    Ok(())
                } else {
                    Err(StompClientError::new(format!(
                        "Handshake failed, server replied with invalid StompVersion: {}",
                        connected_frame.version
                    )))
                }
            }
            Ok(other_frame) => Err(StompClientError::new(format!(
                "Handshake failed, expected CONNECTED Frame, received: {}",
                other_frame
            ))),
            Err(err) => Err(err.into()),
        },
    }
}

fn connect_frame(host_id: String) -> Vec<u8> {
    ConnectFrameBuilder::new(host_id, StompVersions(vec![StompVersion::V1_2]))
        .build()
        .try_into()
        .unwrap()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use futures::channel::mpsc::SendError;
    use futures::future::join;
    use wasm_bindgen_test::*;

    use super::*;
    use futures::sink::SinkExt;
    use futures::stream::StreamExt;
    use futures::TryFutureExt;
    use sender_sink::wrappers::SinkError;
    use sender_sink::wrappers::UnboundedSenderSink;
    use std::convert::TryFrom;
    use stomp_parser::client::*;
    use stomp_parser::server::*;
    use stomp_test_utils::*;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    impl From<SendError> for StompClientError {
        fn from(source: SendError) -> Self {
            StompClientError::new(format!("{:?}", source))
        }
    }

    #[wasm_bindgen_test]
    async fn it_works() {
        let (mut in_sender, in_receiver) = futures::channel::mpsc::unbounded();
        let (out_sender, mut out_receiver) = futures::channel::mpsc::unbounded();

        join(
            SessionEventLoop::start(
                "test".to_owned(),
                in_receiver.boxed(),
                Box::pin(out_sender.sink_err_into::<StompClientError>()),
            ),
            async move {
                let bytes = out_receiver.next().await.expect("receive failed");

                if let Ok(ClientFrame::Connect(_)) = ClientFrame::try_from(bytes) {
                    in_sender.send(Ok(ConnectedFrameBuilder::new(StompVersion::V1_2)
                        .build()
                        .try_into()
                        .expect("serialize failed")));
                } else {
                    panic!("Did not receive connect frame");
                }
            },
        )
        .await;
    }
}
