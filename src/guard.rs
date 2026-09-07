//! Request checks that need no user identity: the `Host` allowlist and
//! the `Origin` check.
//!
//! A browser applies no same-origin policy to a WebSocket handshake, and a
//! page from anywhere may `fetch` a loopback address; only the response is
//! withheld cross-origin, and a write has already happened by then. DNS
//! rebinding goes further: a hostname the attacker controls is re-pointed
//! at `127.0.0.1`, and that page's requests carry the attacker's name in
//! both `Host` and `Origin`, so they look same-origin to the browser too.
//!
//! Two checks close both paths, and neither needs to know who the user is.
//!
//! - `Host` must name this server: an IP literal, `localhost`, a
//!   `.localhost` or `.local` name, the host part of the bind address, or
//!   a hostname listed under `hosts` in the transport config. Anything
//!   else is answered 421. An IP literal cannot be rebound; a name can, so
//!   names are allowlisted.
//! - On a WebSocket upgrade and on every request whose method is not `GET`
//!   or `HEAD`, an `Origin` header must name the host and port the request
//!   was sent to, or a hostname listed under `hosts` (the public name a
//!   tunnel or proxy in front of Mezame may have rewritten out of `Host`).
//!   Anything else is answered 403. A request without `Origin` passes:
//!   browsers attach one to every request these checks cover, so its
//!   absence means a non-browser client.
//!
//! A request with no `Host` and no authority in its URI passes the first
//! check on the same reasoning: a browser sends `Host` always, and hyper
//! refuses an HTTP/1.1 request without one before this layer runs. Such a
//! request still fails the second check if it carries an `Origin`.
//!
//! Reads over plain `GET` carry no `Origin` check. The browser withholds a
//! cross-origin response on its own, and the `Host` check is what stops the
//! rebound page that would otherwise read it as same-origin.

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Mutex, PoisonError};

use axum::{
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::config::{Config, TransportConfig};

/// How many distinct refused hosts and origins are reported on stderr
/// before the reporting stops. A scanning page or a misconfigured proxy
/// costs one line per value, and never one per request.
const NOTED_CAP: usize = 64;

/// How much of a refused header value the refusal echoes back.
const ECHO_LIMIT: usize = 100;

/// The names this server answers to, beyond IP literals and local names.
#[derive(Debug, Default)]
pub struct RequestPolicy {
    /// Hostnames the `Host` check accepts, lowercased and without a port:
    /// the configured `hosts` and the host part of every bind address.
    served: HashSet<String>,
    /// Hostnames an `Origin` may name whatever `Host` says: the configured
    /// `hosts` only, the public names a proxy may have rewritten out of
    /// `Host`. The bind host is not among them. A page on another port of
    /// the bind host is another page, and the equality rule covers a page
    /// on the bind host's own port.
    public: HashSet<String>,
    /// Refused values already reported once.
    noted: Mutex<HashSet<String>>,
}

/// Lowercase each name and drop its port and any blank entry.
fn name_set<I, S>(names: I) -> HashSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .map(|n| normalize_host(host_of(n.as_ref())))
        .filter(|n| !n.is_empty())
        .collect()
}

impl RequestPolicy {
    /// A policy whose public hostnames are `hosts`, accepted by the `Host`
    /// check and trusted as origins. Each entry is a hostname; a port on
    /// it is ignored.
    pub fn new<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let public = name_set(hosts);
        Self {
            served: public.clone(),
            public,
            noted: Mutex::new(HashSet::new()),
        }
    }

    /// The policy a config asks for: every entry under `hosts` as a public
    /// hostname, and the host part of every bind address served as well.
    pub fn from_config(config: &Config) -> Self {
        let mut binds: Vec<&str> = Vec::new();
        let mut hosts: Vec<&str> = Vec::new();
        for transport in &config.transports {
            match transport {
                TransportConfig::Cloudflared {
                    bind,
                    hosts: listed,
                } => {
                    binds.push(bind.as_str());
                    hosts.extend(listed.iter().map(String::as_str));
                }
            }
        }
        let mut policy = Self::new(hosts);
        policy.served.extend(name_set(binds));
        policy
    }

    /// Whether a request sent to `authority` (the `Host` header, host and
    /// optional port) names this server.
    pub fn host_allowed(&self, authority: &str) -> bool {
        let host = normalize_host(host_of(authority));
        !host.is_empty()
            && (is_ip_literal(&host) || is_local_name(&host) || self.served.contains(&host))
    }

    /// Whether a request carrying `origin` and sent to `authority` came
    /// from a page this server itself served.
    ///
    /// The origin's host must equal the request host. When the request
    /// names a port, the origin's port, explicit or implied by its scheme,
    /// must equal it too; when the request names none, a proxy has
    /// dropped it and the host alone decides. An origin whose host is a
    /// configured public hostname passes on that alone.
    pub fn origin_allowed(&self, origin: &str, authority: &str) -> bool {
        let Some((scheme, rest)) = origin.trim().split_once("://") else {
            // `null`, an empty value, or anything that is not a URL.
            return false;
        };
        let origin_authority = rest.split('/').next().unwrap_or("");
        let origin_host = normalize_host(host_of(origin_authority));
        if origin_host.is_empty() {
            return false;
        }
        if self.public.contains(&origin_host) {
            return true;
        }
        if origin_host != normalize_host(host_of(authority)) {
            return false;
        }
        match port_of(authority) {
            None => true,
            Some(request_port) => {
                let origin_port = port_of(origin_authority).or(match scheme {
                    "https" | "wss" => Some(443),
                    "http" | "ws" => Some(80),
                    _ => None,
                });
                origin_port == Some(request_port)
            }
        }
    }

    /// Report a refusal on stderr once per distinct value, up to a cap.
    fn note(&self, line: String) {
        let mut noted = self.noted.lock().unwrap_or_else(PoisonError::into_inner);
        if noted.len() < NOTED_CAP && noted.insert(line.clone()) {
            eprintln!("{line}");
        }
    }
}

/// The host part of `host[:port]` or `[v6]:port`, untrimmed of case.
fn host_of(authority: &str) -> &str {
    let authority = authority.trim();
    if authority.starts_with('[') {
        match authority.find(']') {
            Some(end) => &authority[..=end],
            None => authority,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            _ => authority,
        }
    }
}

/// The port of `host:port` or `[v6]:port`, when one is named.
fn port_of(authority: &str) -> Option<u16> {
    let authority = authority.trim();
    let after_host = if authority.starts_with('[') {
        &authority[authority.find(']')? + 1..]
    } else {
        authority
    };
    let (_, port) = after_host.rsplit_once(':')?;
    port.parse().ok()
}

/// Lowercase, with the trailing dot of a fully qualified name removed.
fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// An IPv4 dotted quad or a bracketed IPv6 address. Neither is subject to
/// DNS, so neither can be rebound.
fn is_ip_literal(host: &str) -> bool {
    host.parse::<Ipv4Addr>().is_ok()
        || host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .is_some_and(|h| h.parse::<Ipv6Addr>().is_ok())
}

/// `localhost`, a `.localhost` name, or a `.local` name. The first two
/// resolve inside the browser and never touch DNS; the third is
/// multicast-only, and no public zone can answer for it.
fn is_local_name(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local")
}

/// The requests the `Origin` check covers: a WebSocket upgrade, and any
/// method other than `GET` and `HEAD`.
fn origin_is_checked(req: &Request) -> bool {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return true;
    }
    req.headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("websocket"))
        })
}

/// The authority the request was sent to: the `Host` header, or the URI's
/// own authority for a client that put it there instead.
fn request_authority(req: &Request) -> Option<String> {
    match req.headers().get(header::HOST) {
        Some(value) => Some(String::from_utf8_lossy(value.as_bytes()).into_owned()),
        None => req.uri().authority().map(|a| a.as_str().to_string()),
    }
}

/// A refused value, cut to a length that keeps the response and the log
/// line readable.
fn echo(value: &str) -> String {
    let mut shown: String = value.chars().take(ECHO_LIMIT).collect();
    if shown.len() < value.len() {
        shown.push('…');
    }
    shown
}

/// The middleware: `Host` first, then `Origin` where it applies.
pub async fn guard_request(
    State(policy): State<std::sync::Arc<RequestPolicy>>,
    req: Request,
    next: Next,
) -> Response {
    let authority = request_authority(&req);
    if authority
        .as_deref()
        .is_some_and(|a| !policy.host_allowed(a))
    {
        let shown = echo(authority.as_deref().unwrap_or_default());
        policy.note(format!(
            "Refused a request for host {shown:?}: not a name this Mezame serves. \
             If it is yours, add it to \"hosts\" in config.json."
        ));
        return (
            StatusCode::MISDIRECTED_REQUEST,
            format!(
                "Host {shown:?} is not a name this Mezame serves. \
                 If it is yours, add it to \"hosts\" in ~/.mezame/config.json.\n"
            ),
        )
            .into_response();
    }

    if origin_is_checked(&req) {
        if let Some(origin) = req.headers().get(header::ORIGIN) {
            let origin = String::from_utf8_lossy(origin.as_bytes()).into_owned();
            let authority = authority.unwrap_or_default();
            if !policy.origin_allowed(&origin, &authority) {
                let shown_origin = echo(&origin);
                let shown_host = echo(&authority);
                policy.note(format!(
                    "Refused a {} from origin {shown_origin:?} sent to {shown_host:?}.",
                    req.method()
                ));
                return (
                    StatusCode::FORBIDDEN,
                    format!(
                        "Origin {shown_origin:?} does not match the host {shown_host:?} \
                         this request was sent to.\n"
                    ),
                )
                    .into_response();
            }
        }
    }

    next.run(req).await
}
