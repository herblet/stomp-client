use std::{
    convert::{TryFrom, TryInto},
    pin::Pin,
};

use async_executors::SpawnHandleExt;
use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver},
    future::ready,
    stream::{empty, select_all, BoxStream},
    task::{LocalSpawn, SpawnExt},
    Future, FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt, TryStreamExt,
};
use stomp_parser::{
    client::{ClientFrame, ConnectFrameBuilder},
    headers::{StompVersion, StompVersions},
    server::ServerFrame,
};

use crate::{error::StompClientError, session::StompClientSession};

pub type BytesStream = BoxStream<'static, Result<Vec<u8>, StompClientError>>;
pub type BytesSink = Pin<Box<dyn Sink<Vec<u8>, Error = StompClientError> + Sync + Send + 'static>>;

pub enum SessionEvent {
    ServerFrame(Result<ServerFrame, StompClientError>),
    ServerHeartBeat,
    Close,
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

pub struct SessionEventLoop {}

impl SessionEventLoop {
    fn new() -> SessionEventLoop {
        SessionEventLoop {}
    }

    pub async fn start(
        host_id: String,
        stream_from_server: BytesStream,
        mut sink_to_server: BytesSink,
    ) -> Result<StompClientSession, StompClientError> {
        let remaining_stream = sink_to_server
            .send(connect_frame(host_id))
            .and_then(|_| head(stream_from_server))
            .and_then(move |(first, remaining_stream)| async {
                validate_handshake(first).map(|_| remaining_stream)
            })
            .await?;

        let stream_from_server = create_server_event_stream(remaining_stream);

        let (client_event_sender, stream_from_client) = unbounded();

        let client_heartbeat_stream = empty();

        let all_events = select_all(vec![
            stream_from_client.boxed(),
            stream_from_server.boxed(),
            client_heartbeat_stream.boxed(),
        ]);

        let event_handler = {
            let mut session = SessionEventLoop::new();

            // client_session.start_heartbeat_listener(client_heartbeat_stream);

            move |event| session.handle_event(event)
        };

        let response_stream = all_events
            .then(event_handler)
            .inspect_ok(|_| {
                log::debug!("Response");
            })
            .inspect_err(Self::log_error)
            .take_while(Self::not_dead);

        crate::executor()
            .spawn(
                response_stream
                    .filter_map(Self::into_opt_ok_of_bytes)
                    .forward(sink_to_server)
                    .map(|_| ()),
            )
            .expect("Event loop init failed");

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

    fn handle_event(&mut self, event: SessionEvent) -> EventResultFuture {
        todo!()
    }
}

fn create_server_event_stream(raw_stream: BytesStream) -> impl Stream<Item = SessionEvent> {
    // Closes this session; will be chained to client stream to run after that ends
    let close_stream = futures::stream::once(async { SessionEvent::Close }).boxed();

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

fn head(
    stream_from_server: Pin<Box<dyn Stream<Item = Result<Vec<u8>, StompClientError>> + Send>>,
) -> impl Future<
    Output = Result<
        (
            Option<Vec<u8>>,
            Pin<Box<dyn Stream<Item = Result<Vec<u8>, StompClientError>> + Send>>,
        ),
        StompClientError,
    >,
> {
    stream_from_server
        .into_future()
        .map(|(first_res, remainder)| first_res.transpose().map(|bytes| (bytes, remainder)))
}

fn connect_frame(host_id: String) -> Vec<u8> {
    ConnectFrameBuilder::new(host_id, StompVersions(vec![StompVersion::V1_2]))
        .build()
        .try_into()
        .unwrap()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
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

    impl From<SinkError> for StompClientError {
        fn from(source: SinkError) -> Self {
            StompClientError::new(format!("{:?}", source))
        }
    }

    fn session_factory(
        in_receiver: InReceiver<StompClientError>,
        out_sender: OutSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), StompClientError>> + Send>> {
        SessionEventLoop::start(
            "test".to_owned(),
            UnboundedReceiverStream::new(in_receiver).boxed(),
            Box::pin(UnboundedSenderSink::from(out_sender).sink_err_into::<StompClientError>()),
        )
        .map_ok(|_session| ())
        .boxed()
    }
    #[tokio::test]
    async fn it_works() {
        assert_behaviour(
            session_factory,
            receive(|bytes| matches!(ClientFrame::try_from(bytes), Ok(ClientFrame::Connect(_))))
                .then(send(ServerFrame::Connected(
                    ConnectedFrameBuilder::new(StompVersion::V1_2).build(),
                ))),
        )
        .await;
    }
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
