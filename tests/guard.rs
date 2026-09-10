//! The `Host` allowlist and the `Origin` check as pure decisions, apart
//! from the router, and the middleware's choice of the host it compares an
//! `Origin` with. `tests/http_routes.rs` and `tests/ws_upgrade.rs` cover
//! the same policy wired in front of the routes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{middleware, Router};
use mezame::config::{Config, TransportConfig};
use mezame::guard::{guard_request, RequestPolicy};
use tower::ServiceExt;

/// A policy with nothing configured: IP literals and local names only.
fn bare() -> RequestPolicy {
    RequestPolicy::new(Vec::<&str>::new())
}

fn with(names: &[&str]) -> RequestPolicy {
    RequestPolicy::new(names.iter().copied())
}

#[test]
fn ip_literals_and_local_names_are_always_served() {
    // None of these is subject to DNS, so none can be rebound: a page at
    // one of them is the page this server served.
    let policy = bare();
    for host in [
        "127.0.0.1:9510",
        "127.0.0.1",
        "[::1]:9510",
        "[::1]",
        "192.168.1.20:9510",
        "10.0.0.7",
        "0.0.0.0:9510",
        "localhost:9510",
        "localhost",
        "LOCALHOST:9510",
        "localhost.",
        "app.localhost:9510",
        "stefans-mac.local:9510",
        "stefans-mac.local.",
    ] {
        assert!(policy.host_allowed(host), "{host:?} names this server");
    }
}

#[test]
fn a_hostname_is_served_only_when_configured() {
    let policy = bare();
    for host in [
        "attacker.example:9510",
        "mezame.example.com",
        "example.com",
        "localhost.attacker.example",
        "local.attacker.example",
        "127.0.0.1.attacker.example",
        "",
        ":9510",
        "[::1",
        "1.2.3",
        "::1",
    ] {
        assert!(
            !policy.host_allowed(host),
            "{host:?} is refused with nothing configured"
        );
    }

    let policy = with(&["mezame.example.com"]);
    assert!(policy.host_allowed("mezame.example.com"));
    assert!(
        policy.host_allowed("MEZAME.example.com:443"),
        "case and port are not part of the name"
    );
    assert!(
        policy.host_allowed("mezame.example.com."),
        "a trailing dot is the same name"
    );
    assert!(
        !policy.host_allowed("evil.mezame.example.com"),
        "a subdomain is another name"
    );
    assert!(
        !policy.host_allowed("attacker.example"),
        "the list is exact"
    );
}

#[test]
fn the_bind_host_and_the_configured_hosts_come_from_the_config() {
    let config = Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "mezame.lan:9510".to_string(),
            hosts: vec!["Mezame.Example.com:443".to_string(), " ".to_string()],
        }],
        version: 2,
        datastore: Default::default(),
        public_url: None,
        models: vec![],
    };
    let policy = RequestPolicy::from_config(&config);
    assert!(
        policy.host_allowed("mezame.lan:9510"),
        "the bind host is a name clients use"
    );
    assert!(
        policy.host_allowed("mezame.example.com"),
        "a configured host, whatever its case and port in the file"
    );
    assert!(!policy.host_allowed(""), "a blank entry allows nothing");
    assert!(!policy.host_allowed("other.lan:9510"));

    // The bind host is served, and a page on its own port is the page this
    // server served, but a page on another of its ports is another page:
    // only a configured public hostname is trusted whatever `Host` says.
    assert!(policy.origin_allowed("http://mezame.lan:9510", "mezame.lan:9510"));
    assert!(
        !policy.origin_allowed("http://mezame.lan:8080", "mezame.lan:9510"),
        "the bind host is not a trusted origin on another port"
    );
    assert!(
        !policy.origin_allowed("http://mezame.lan:9510", "127.0.0.1:9510"),
        "nor whatever Host says"
    );
    assert!(policy.origin_allowed("https://mezame.example.com", "127.0.0.1:9510"));
}

#[test]
fn an_origin_matching_the_request_host_and_port_passes() {
    let policy = bare();
    for (origin, host) in [
        ("http://127.0.0.1:9510", "127.0.0.1:9510"),
        ("http://localhost:9510", "localhost:9510"),
        ("http://[::1]:9510", "[::1]:9510"),
        ("HTTP://LOCALHOST:9510", "localhost:9510"),
        // A tunnel passes the public hostname through in `Host`, with no
        // port, and the browser's `Origin` carries none either.
        ("https://mezame.example.com", "mezame.example.com"),
        ("https://mezame.example.com:443", "mezame.example.com"),
        // A port the scheme implies matches one the request spells out.
        ("http://127.0.0.1", "127.0.0.1"),
        ("http://127.0.0.1", "127.0.0.1:80"),
        ("https://127.0.0.1", "127.0.0.1:443"),
        ("http://mezame.example.com", "mezame.example.com:80"),
    ] {
        assert!(
            policy.origin_allowed(origin, host),
            "{origin:?} against {host:?} is the page this server served"
        );
    }
}

#[test]
fn an_origin_from_anywhere_else_is_refused() {
    let policy = bare();
    for (origin, host) in [
        ("http://evil.example", "127.0.0.1:9510"),
        ("http://evil.example:9510", "127.0.0.1:9510"),
        // Another port on the same host is another page.
        ("http://127.0.0.1:8080", "127.0.0.1:9510"),
        ("http://127.0.0.1", "127.0.0.1:9510"),
        ("https://127.0.0.1", "127.0.0.1:9510"),
        // The shipped UI builds its socket URL from `location.host`, so
        // the two names never differ for a page this server served.
        ("http://localhost:9510", "127.0.0.1:9510"),
        ("http://127.0.0.1.evil.example:9510", "127.0.0.1:9510"),
        // A sandboxed frame or a `file://` page.
        ("null", "127.0.0.1:9510"),
        ("", "127.0.0.1:9510"),
        ("127.0.0.1:9510", "127.0.0.1:9510"),
        ("http://", "127.0.0.1:9510"),
        ("http://127.0.0.1:9510", ""),
    ] {
        assert!(
            !policy.origin_allowed(origin, host),
            "{origin:?} against {host:?} is another page"
        );
    }
}

/// A `POST` carrying `origin`, sent to `host`, with `X-Forwarded-Host` set
/// to `forwarded` when given, through one route behind the guard: the
/// status the middleware answers, 204 when it let the write through.
async fn post_through_guard(host: &str, forwarded: Option<&str>, origin: &str) -> StatusCode {
    let app = Router::new()
        .route("/write", post(|| async { StatusCode::NO_CONTENT }))
        .layer(middleware::from_fn_with_state(
            Arc::new(bare()),
            guard_request,
        ));
    let mut req = Request::post("/write")
        .header("host", host)
        .header("origin", origin);
    if let Some(forwarded) = forwarded {
        req = req.header("x-forwarded-host", forwarded);
    }
    app.oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("the router answers")
        .status()
}

#[tokio::test]
async fn a_forwarded_host_is_compared_without_its_port_and_host_with_it() {
    // Phase 2 Requirement 6 criterion 3: the compared host is
    // `X-Forwarded-Host` when present, host-only. A proxy that writes its
    // own listener's port into the header must not fail the page it
    // serves, and a bracketed IPv6 host keeps its brackets and loses only
    // the port after them. `Host` keeps the phase 0 rule, port included, so
    // a page on another port of the same host stays another page.
    let host = "127.0.0.1:9510";
    assert_eq!(
        post_through_guard(host, Some("app.example:8443"), "https://app.example").await,
        StatusCode::NO_CONTENT,
        "the forwarded port is not compared"
    );
    assert_eq!(
        post_through_guard(host, Some("other.example"), "https://app.example").await,
        StatusCode::FORBIDDEN,
        "the forwarded host still is"
    );
    assert_eq!(
        post_through_guard(host, Some("[::1]:8080"), "http://[::1]:3000").await,
        StatusCode::NO_CONTENT,
        "a bracketed IPv6 host loses only its port"
    );
    assert_eq!(
        post_through_guard(host, None, "http://127.0.0.1:8080").await,
        StatusCode::FORBIDDEN,
        "Host keeps its port"
    );
    assert_eq!(
        post_through_guard(host, None, "http://127.0.0.1:9510").await,
        StatusCode::NO_CONTENT
    );
}

#[test]
fn a_configured_hostname_is_a_trusted_origin_whatever_host_says() {
    // A proxy that rewrites `Host` to the bind address still forwards the
    // browser's `Origin` untouched.
    let policy = with(&["mezame.example.com"]);
    assert!(policy.origin_allowed("https://mezame.example.com", "127.0.0.1:9510"));
    assert!(policy.origin_allowed("https://MEZAME.example.com", "localhost:9510"));
    assert!(!policy.origin_allowed("https://evil.example", "127.0.0.1:9510"));
    assert!(!policy.origin_allowed("https://mezame.example.com.evil.example", "127.0.0.1:9510"));
}
