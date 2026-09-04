//! Test for `mezame::http::enable_tcp_keepalive`.
//!
//! TCP keepalive on the listening socket is the kernel-level backstop to
//! the application heartbeat in `src/ws.rs`. Accepted connections inherit
//! the listener's setting on Linux, and a half-open socket the kernel can
//! detect is torn down even with the ping task wedged. See GitHub issue #4.
//!
//! The function logs and swallows a failure by design, and a silent no-op
//! would leave dead sockets `ESTABLISHED` forever with nothing to show for
//! it. This reads the flag back off the socket to prove it took.

use mezame::http::enable_tcp_keepalive;
use socket2::SockRef;
use tokio::net::TcpListener;

#[tokio::test]
async fn keepalive_is_readable_back_off_the_listener() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");

    assert!(
        !SockRef::from(&listener)
            .keepalive()
            .expect("read keepalive before"),
        "a fresh listener should start without keepalive"
    );

    enable_tcp_keepalive(&listener);

    assert!(
        SockRef::from(&listener)
            .keepalive()
            .expect("read keepalive after"),
        "enable_tcp_keepalive must leave SO_KEEPALIVE set on the listener"
    );
}
