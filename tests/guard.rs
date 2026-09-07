//! The `Host` allowlist and the `Origin` check as pure decisions, apart
//! from the router. `tests/http_routes.rs` and `tests/ws_upgrade.rs` cover
//! the same policy wired in front of the routes.

use mezame::config::{Config, TransportConfig};
use mezame::guard::RequestPolicy;

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
