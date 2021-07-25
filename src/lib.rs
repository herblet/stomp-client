use futures::future::{Future, FutureExt, TryFutureExt};
use futures::sink::{Sink, SinkExt};
use futures::stream::{BoxStream, StreamExt};
use futures::Stream;
use std::convert::{TryFrom, TryInto};
use std::pin::Pin;
use stomp_parser::client::ConnectFrameBuilder;
use stomp_parser::error::StompParseError;
use stomp_parser::headers::{StompVersion, StompVersions};
use stomp_parser::server::ServerFrame;

pub struct StompClientError {
    message: String,
}

impl StompClientError {
    fn new<T: Into<String>>(message: T) -> Self {
        StompClientError {
            message: message.into(),
        }
    }
}

impl std::fmt::Debug for StompClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("StompClientError: {}", self.message))
    }
}

impl From<StompParseError> for StompClientError {
    fn from(cause: StompParseError) -> Self {
        StompClientError::new(cause.message())
    }
}

pub type BytesStream = BoxStream<'static, Result<Vec<u8>, StompClientError>>;
pub type BytesSink = Pin<Box<dyn Sink<Vec<u8>, Error = StompClientError> + Sync + Send + 'static>>;

pub struct StompClientSession {}

impl StompClientSession {
    pub async fn start(
        host_id: String,
        stream_from_server: BytesStream,
        mut sink_to_server: BytesSink,
    ) -> Result<StompClientSession, StompClientError> {
        sink_to_server
            .send(connect_frame(host_id))
            .and_then(|_| head(stream_from_server))
            .and_then(move |(first, _remainder)| async {
                validate_handshake(first)?;
                Ok(StompClientSession {})
            })
            .await
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::sink::SinkExt;
    use futures::stream::StreamExt;
    use sender_sink::wrappers::UnboundedSenderSink;
    use std::convert::TryFrom;
    use stomp_parser::client::*;
    use stomp_parser::server::*;
    use stomp_test_utils::*;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    use sender_sink::wrappers::SinkError;

    impl From<SinkError> for StompClientError {
        fn from(source: SinkError) -> Self {
            StompClientError {
                message: format!("{:?}", source),
            }
        }
    }

    fn session_factory(
        in_receiver: InReceiver<StompClientError>,
        out_sender: OutSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), StompClientError>> + Send>> {
        StompClientSession::start(
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
