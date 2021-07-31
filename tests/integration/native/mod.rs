
use futures::channel::oneshot;
use futures::future::ready;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::Future;
use futures::FutureExt;
use sender_sink::wrappers::UnboundedSenderSink;
use std::convert::TryFrom;
use std::convert::TryInto;
use std::pin::Pin;
use stomp_client::error::*;
use stomp_client::event_loop::*;
use stomp_client::session::StompClientSession;
use stomp_client::spawn;
use stomp_parser::client::*;
use stomp_parser::error::StompParseError;
use stomp_parser::headers::StompVersion;
use stomp_parser::server::*;
use stomp_test_utils::*;
use tokio::task::yield_now;
use tokio_stream::wrappers::UnboundedReceiverStream;

fn session_factory(
    in_receiver: InReceiver<StompClientError>,
    out_sender: OutSender,
) -> Pin<Box<dyn Future<Output = Result<StompClientSession, StompClientError>> + Send>> {
    SessionEventLoop::start(
        "test".to_owned(),
        UnboundedReceiverStream::new(in_receiver).boxed(),
        Box::pin(UnboundedSenderSink::from(out_sender).sink_err_into::<StompClientError>()),
    )
    .boxed()
}
#[tokio::test]
async fn sends_connect_frame() {
    let _ = env_logger::try_init();

    let (session_sender, session_receiver) = oneshot::channel();

    let factory = |in_receiver, out_sender: OutSender| {
        spawn(async move {
            session_sender
                .send(
                    session_factory(in_receiver, out_sender)
                        .await
                        .expect("Session init failed"),
                )
                .expect("Send failed");
            yield_now().await;
        });
        ready(Ok(())).boxed()
    };

    assert_behaviour(
        factory,
        into_behaviour(move |in_sender, out_receiver| {
            async move {
                let result = out_receiver
                    .recv()
                    .await
                    .map(ClientFrame::try_from)
                    .expect("No message")
                    .expect("Not a frame");

                if let ClientFrame::Connect(_) = result {
                    in_sender
                        .send(
                            ServerFrame::Connected(
                                ConnectedFrameBuilder::new(StompVersion::V1_2).build(),
                            )
                            .try_into()
                            .map_err(|err: StompParseError| err.into()),
                        )
                        .expect("Send failed");
                } else {
                    panic!("Not a Connect frame");
                }
                let session = session_receiver.await.expect("Session creation failed");

                let mut foo_stream = session.subscribe("foo".to_owned()).expect("Subscribe failed");

                let mut subscription_id = "".to_owned();

                yield_now().await;

                assert_receive(out_receiver, |bytes| matches!(ClientFrame::try_from(bytes), Ok(ClientFrame::Subscribe(frame)) if frame.destination.value() == "foo".to_owned()
                    && Some(subscription_id = frame.id.value().to_owned()).map(|_|true).unwrap()));

                send_data(in_sender, ServerFrame::Message(MessageFrameBuilder::new("1".into(),"foo".into(),subscription_id.clone()).body(b"Hello, world!".to_vec()).build()));

                yield_now().await;

                let message = foo_stream.next().await.expect("Stream closed prematurely");

                assert_eq!(b"Hello, world!", message.body().expect("Message has no body"));

                drop(foo_stream);
                
                yield_now().await;
                yield_now().await;

                assert_receive(out_receiver, |bytes| matches!(ClientFrame::try_from(bytes), Ok(ClientFrame::Unsubscribe(frame)) if frame.id.value() == subscription_id));

                session.send("bar".to_owned(), b"Goodbye, moon!".to_vec()).expect("Send failed");

                yield_now().await;

                assert_receive(out_receiver, |bytes| matches!(ClientFrame::try_from(bytes), Ok(ClientFrame::Send(frame)) if frame.destination.value() == "bar" && frame.body() == Some(b"Goodbye, moon!")));

                drop(session);

                yield_now().await;

                assert!(matches!(out_receiver.recv().await,None), "Outbound connection not closed");

            }
            .boxed()
        }),
    )
    .await;
}
