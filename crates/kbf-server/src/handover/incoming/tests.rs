//! A successor asking a frontend that serves to hand over, against a holder the test
//! plays.

use std::future::Future;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::AsFd;
use std::path::PathBuf;

use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use super::*;
use crate::handover::tests::Scratch;

fn hello() -> Message {
    Message::Hello {
        version: crate::handover::wire::PROTOCOL,
        reapi: "127.0.0.1:1".parse().expect("address"),
        worker: "127.0.0.1:2".parse().expect("address"),
        api: None,
        state: PathBuf::from("/state.db"),
    }
}

fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let local = listener.local_addr().expect("address");
    (listener, local)
}

/// A holder at `path` that plays `script` with the first connection.
fn holder<F, Fut>(path: &Path, script: F) -> JoinHandle<()>
where
    F: FnOnce(Link) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let listener = UnixListener::bind(path).expect("a control socket");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a connection");
        script(Link::new(stream)).await;
    })
}

/// Catches: a start that fails, or waits, when no frontend serves: no socket file (the
/// first start of a machine), a socket file nobody listens on (a frontend that died), are
/// both "nobody serves"; any other failure of the socket (here a path no socket can have)
/// is an error and not a cold start (which could bind the listeners of a frontend that
/// is serving).
#[tokio::test]
async fn nobody_serving_is_a_start_with_no_predecessor() {
    let dir = Scratch::new();
    let path = dir.socket();
    assert!(knock(&path, &hello()).await.expect("no socket").is_none());

    drop(std::os::unix::net::UnixListener::bind(&path).expect("a socket nobody serves"));
    assert!(path.exists());
    assert!(
        knock(&path, &hello())
            .await
            .expect("a stale socket")
            .is_none()
    );

    let too_long = dir.0.join("x".repeat(200));
    let error = knock(&too_long, &hello()).await.err().expect("an error");
    assert!(matches!(error, HandoverError::Socket(_)), "{error}");
}

/// Catches: sockets taken for other sockets or in another order, an offer that does not
/// carry the generation it names, a hello that does not reach the holder as it was, and
/// a `ready` that is not answered by `released`: the successor holds the same sockets the
/// holder offered, for the two shapes an offer has (with and without the operator API),
/// and `ready` returns once the holder says it let go.
#[tokio::test]
async fn an_offer_gives_the_sockets_and_the_generation() {
    for api in [false, true] {
        let dir = Scratch::new();
        let (reapi, reapi_addr) = listener();
        let (worker, worker_addr) = listener();
        let (operator, operator_addr) = listener();
        let asked = hello();
        let expected = asked.clone();
        let served = holder(&dir.socket(), move |mut link| async move {
            assert_eq!(link.hear().await.expect("hello"), expected);
            let mut sockets = vec![reapi.as_fd(), worker.as_fd()];
            if api {
                sockets.push(operator.as_fd());
            }
            let offer = Message::Offer { generation: 9, api };
            link.say_with(&offer, &sockets).await.expect("offer");
            assert_eq!(link.hear().await.expect("ready"), Message::Ready);
            link.say(&Message::Released).await.expect("released");
        });
        let (offer, sockets) = knock(&dir.socket(), &asked)
            .await
            .expect("an offer")
            .expect("a frontend serves");
        assert_eq!(offer.generation, 9);
        let local = |fd| TcpListener::from(fd).local_addr().expect("a listener");
        assert_eq!(local(sockets.reapi), reapi_addr);
        assert_eq!(local(sockets.worker), worker_addr);
        assert_eq!(sockets.api.map(local), api.then_some(operator_addr));
        offer.ready().await.expect("the holder let go");
        served.await.expect("the holder");
    }
}

/// Catches: an offer taken whose sockets are not the ones it announces (too few: the
/// successor would serve with a socket missing; too many: one would be dropped unseen),
/// a refusal taken for an offer, and any other first answer taken for one.
#[tokio::test]
async fn an_offer_that_is_not_one_is_an_error() {
    let cases: [(Message, usize, &str); 5] = [
        (
            Message::Offer {
                generation: 1,
                api: true,
            },
            2,
            "carried 2 sockets, not the 3",
        ),
        (
            Message::Offer {
                generation: 1,
                api: false,
            },
            3,
            "carried 3 sockets, not the 2",
        ),
        (
            Message::Offer {
                generation: 1,
                api: false,
            },
            1,
            "carried 1 sockets, not the 2",
        ),
        (
            Message::Refused("other flags".to_owned()),
            0,
            "refused the handover: other flags",
        ),
        (Message::Ready, 0, "unexpected handover message: ready"),
    ];
    for (answer, carried, said) in cases {
        let dir = Scratch::new();
        let held: Vec<_> = (0..carried).map(|_| listener().0).collect();
        let served = holder(&dir.socket(), move |mut link| async move {
            link.hear().await.expect("hello");
            let sockets: Vec<_> = held.iter().map(AsFd::as_fd).collect();
            link.say_with(&answer, &sockets).await.expect("answer");
        });
        let error = knock(&dir.socket(), &hello())
            .await
            .err()
            .expect("an error");
        assert!(error.to_string().contains(said), "{error}");
        served.await.expect("the holder");
    }
}

/// Catches: a successor that goes on when the holder could not stand down (it would run
/// beside a holder that still writes), or when the holder answers with something else
/// or goes away: each ends `ready` with an error that says which.
#[tokio::test]
async fn ready_ends_in_an_error_unless_the_holder_says_released() {
    let answers = [
        Some(Message::Failed("the writer cannot stop".to_owned())),
        Some(Message::Ready),
        None,
    ];
    let wanted = [
        "could not stand down: the writer cannot stop",
        "unexpected handover message: ready",
        "closed the control connection",
    ];
    for (answer, wanted) in answers.into_iter().zip(wanted) {
        let dir = Scratch::new();
        let (reapi, _) = listener();
        let (worker, _) = listener();
        let served = holder(&dir.socket(), move |mut link| async move {
            link.hear().await.expect("hello");
            let offer = Message::Offer {
                generation: 1,
                api: false,
            };
            link.say_with(&offer, &[reapi.as_fd(), worker.as_fd()])
                .await
                .expect("offer");
            link.hear().await.expect("ready");
            if let Some(answer) = answer {
                link.say(&answer).await.expect("answer");
            }
        });
        let (offer, _sockets) = knock(&dir.socket(), &hello())
            .await
            .expect("an offer")
            .expect("a frontend serves");
        let error = offer.ready().await.expect_err("not released");
        assert!(error.to_string().contains(wanted), "{error}");
        served.await.expect("the holder");
    }
}
