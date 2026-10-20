//! Regression for stale HTTP/1 readiness pooling a live SSE connection.
//! See hyperium/hyper#4207. A real Hyper driver and Tokio I/O are polled with
//! controlled cooperative-budget exhaustion; no wall-clock race is required.
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

#[tokio::test]
async fn http1_dispatch_withdraws_stale_readiness_after_cooperative_yield() {
    let (io, _peer) = tokio::io::duplex(4096);
    let (mut sender, mut connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
            .await
            .unwrap();
    poll_fn(|cx| {
        assert!(Pin::new(&mut connection).poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(sender.is_ready());
    // Keep the response future alive so the request is not cancelled.
    let _response = sender.send_request(hyper::Request::new(axum::body::Body::empty()));
    assert!(!sender.is_ready());
    poll_fn(|cx| {
        while let Poll::Ready(restore) = tokio::task::coop::poll_proceed(cx) {
            restore.made_progress();
        }
        assert!(Pin::new(&mut connection).poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::task::yield_now().await;
    poll_fn(|cx| {
        assert!(Pin::new(&mut connection).poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(
        !sender.is_ready(),
        "an in-flight HTTP/1 connection must not be offered to the pool"
    );
}
