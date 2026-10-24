//! The operator API that is sent GOAWAY and goes on answering ([`serve_api_goaway`]): the
//! two handles on one socket, the second server that takes over the accepting, and the end
//! at the stop. That every connection open is sent GOAWAY is read off the wire by the
//! frontend's drain tests (`tests/frontend/drain.rs`).

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::listen::{Binder, Socket};
use crate::serve::serve_api_goaway;
use crate::stop::Stop;

fn routes() -> Router {
    Router::new().route("/ping", get(|| async { "pong" }))
}

/// The status code of one HTTP/1.1 `GET /ping` to `to`, or none if the call failed.
async fn ping(to: SocketAddr) -> Option<u16> {
    let mut tcp = TcpStream::connect(to).await.ok()?;
    let request = b"GET /ping HTTP/1.1\r\nhost: kbf\r\nconnection: close\r\n\r\n";
    tcp.write_all(request).await.ok()?;
    let mut reply = String::new();
    tcp.read_to_string(&mut reply).await.ok()?;
    reply.split_whitespace().nth(1)?.parse().ok()
}

/// Whether `to` answers a call within a second, trying again after a failed one: a
/// connection accepted in the instant the first server stops accepting may be closed at once.
async fn answers(to: SocketAddr) -> bool {
    for _ in 0..50 {
        if ping(to).await == Some(200) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Asserts that an operator API answers before it is sent GOAWAY and after (the second
/// server answers on the same address), and that it ends once told to stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_api_answers_before_and_after_goaway_and_ends_at_the_stop() {
    let any: SocketAddr = "127.0.0.1:0".parse().expect("address");
    let socket = Socket::bind(any, &Binder::new()).expect("bind");
    let at = socket.local;
    let listeners = socket.http_pair().expect("registered");
    let goaway = Stop::new();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(serve_api_goaway(
        Some(listeners),
        routes(),
        goaway.clone(),
        async move {
            let _ = stopped.await;
        },
        None,
    ));

    assert_eq!(ping(at).await, Some(200), "no answer before GOAWAY");
    goaway.fire();
    assert!(answers(at).await, "no answer after GOAWAY");
    stop.send(()).expect("still serving");
    let ended = tokio::time::timeout(Duration::from_secs(5), served).await;
    ended
        .expect("ends at the stop")
        .expect("task")
        .expect("served");
}

/// Asserts that an operator API with no listener only waits for the stop.
#[tokio::test]
async fn without_a_listener_it_waits_for_the_stop_only() {
    let stop = async {};
    let served = serve_api_goaway(None, routes(), Stop::new(), stop, None).await;
    assert!(served.is_ok());
}
