//! Validate browser provenance without trusting forwarded headers from clients.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Uri, header, uri::Authority};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::errors::AppError;
use crate::server::AppState;

#[derive(Debug, Clone, Copy)]
pub struct RequestSecurity {
    pub secure_cookie: bool,
    pub client_ip: Option<IpAddr>,
    pub onion: bool,
}

/// Marks the private Arti forwarding listener so remote onion clients can
/// never impersonate a trusted loopback reverse proxy.
#[derive(Debug, Clone, Copy)]
pub struct OnionTransport;

#[derive(Debug)]
enum Rejection {
    Missing,
    Invalid,
    CrossOrigin,
}

fn singleton<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, Rejection> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Rejection::Invalid);
    }
    let value = value.to_str().map_err(|_| Rejection::Invalid)?;
    if value.is_empty() || value.trim() != value {
        return Err(Rejection::Invalid);
    }
    Ok(Some(value))
}

fn authority(value: &str) -> Result<Authority, Rejection> {
    let authority = value.parse::<Authority>().map_err(|_| Rejection::Invalid)?;
    if authority.host().is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'@' | b'\\' | b',' | b'%'))
    {
        return Err(Rejection::Invalid);
    }
    let suffix = value
        .strip_prefix(authority.host())
        .ok_or(Rejection::Invalid)?;
    if !suffix.is_empty() {
        let port = suffix.strip_prefix(':').ok_or(Rejection::Invalid)?;
        if port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().is_err()
        {
            return Err(Rejection::Invalid);
        }
    }
    Ok(authority)
}

fn same_host(left: &str, right: &str) -> bool {
    let address = |host: &str| host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
    match (address(left), address(right)) {
        (Some(left), Some(right)) => left == right,
        _ => left.eq_ignore_ascii_case(right),
    }
}

fn same_authority(left: &Authority, right: &Authority, https: bool) -> bool {
    let default = if https { 443 } else { 80 };
    same_host(left.host(), right.host())
        && left.port_u16().unwrap_or(default) == right.port_u16().unwrap_or(default)
}

fn trusted_proxy(peer: IpAddr, networks: &[String]) -> bool {
    networks.iter().any(|network| {
        let (host, prefix) = network
            .split_once('/')
            .map_or((network.as_str(), None), |(host, bits)| (host, Some(bits)));
        let Ok(host) = host.parse::<IpAddr>() else {
            return false;
        };
        let prefix = prefix.and_then(|prefix| prefix.parse::<u32>().ok());
        let peer = match (peer, host) {
            (IpAddr::V6(peer), IpAddr::V4(_)) => {
                peer.to_ipv4_mapped().map_or(IpAddr::V6(peer), IpAddr::V4)
            }
            _ => peer,
        };
        match (peer, host) {
            (IpAddr::V4(peer), IpAddr::V4(host)) => {
                let prefix = prefix.unwrap_or(32);
                prefix == 0
                    || (prefix <= 32
                        && u32::from(peer) >> (32 - prefix) == u32::from(host) >> (32 - prefix))
            }
            (IpAddr::V6(peer), IpAddr::V6(host)) => {
                let prefix = prefix.unwrap_or(128);
                prefix == 0
                    || (prefix <= 128
                        && u128::from(peer) >> (128 - prefix) == u128::from(host) >> (128 - prefix))
            }
            _ => false,
        }
    })
}

fn forwarded_client_ip(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    proxy: bool,
    trusted: &[String],
) -> Result<Option<IpAddr>, Rejection> {
    if !proxy {
        return Ok(peer);
    }
    let Some(forwarded) = singleton(headers, "x-forwarded-for")? else {
        return Ok(peer);
    };
    let mut addresses = Vec::new();
    for part in forwarded.split(',') {
        if addresses.len() >= 16 {
            return Err(Rejection::Invalid);
        }
        addresses.push(
            part.trim()
                .parse::<IpAddr>()
                .map_err(|_| Rejection::Invalid)?,
        );
    }
    let mut client = peer;
    for address in addresses.into_iter().rev() {
        client = Some(address);
        if !trusted_proxy(address, trusted) {
            break;
        }
    }
    Ok(client)
}

fn destination(
    state: &AppState,
    request: &Request,
) -> Result<(Authority, RequestSecurity), Rejection> {
    let host = singleton(request.headers(), "host")?
        .map(authority)
        .transpose()?;
    let uri_authority = request
        .uri()
        .authority()
        .map(|value| authority(value.as_str()))
        .transpose()?;
    let destination = uri_authority
        .as_ref()
        .or(host.as_ref())
        .ok_or(Rejection::Invalid)?
        .clone();
    let onion_transport = request.extensions().get::<OnionTransport>().is_some();
    let onion = onion_transport
        || destination
            .host()
            .rsplit_once('.')
            .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("onion"));
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip());
    let proxy = !onion
        && peer.is_some_and(|peer| trusted_proxy(peer, &state.settings.server.trusted_proxy_cidrs));
    let forwarded = if proxy {
        singleton(request.headers(), "x-forwarded-proto")?
    } else {
        None
    };
    let client_ip = forwarded_client_ip(
        request.headers(),
        peer,
        proxy,
        &state.settings.server.trusted_proxy_cidrs,
    )?;
    let https = match forwarded {
        Some("https") => true,
        Some("http") => false,
        Some(_) => return Err(Rejection::Invalid),
        None => !onion && state.settings.server.cookie_secure,
    };
    if let (Some(host), Some(uri)) = (host, uri_authority)
        && !same_authority(&host, &uri, https)
    {
        return Err(Rejection::Invalid);
    }
    Ok((
        destination,
        RequestSecurity {
            secure_cookie: https,
            client_ip,
            onion: onion_transport,
        },
    ))
}

fn check_provenance(
    headers: &HeaderMap,
    destination: &Authority,
    https: bool,
) -> Result<(), Rejection> {
    let site = singleton(headers, "sec-fetch-site")?;
    if site.is_some_and(|site| site.eq_ignore_ascii_case("cross-site")) {
        return Err(Rejection::CrossOrigin);
    }
    let mut supplied = false;
    for (name, is_origin) in [("origin", true), ("referer", false)] {
        let Some(value) = singleton(headers, name)? else {
            continue;
        };
        if is_origin && value == "null" {
            continue;
        }
        let uri = value.parse::<Uri>().map_err(|_| Rejection::Invalid)?;
        let scheme = uri.scheme_str().ok_or(Rejection::Invalid)?;
        let source = uri.authority().ok_or(Rejection::Invalid)?;
        let source = authority(source.as_str())?;
        if value.contains('#')
            || !matches!(scheme, "http" | "https")
            || (is_origin && value.split_once("://").map(|(_, rest)| rest) != Some(source.as_str()))
        {
            return Err(Rejection::Invalid);
        }
        if (scheme == "https") != https || !same_authority(&source, destination, https) {
            return Err(Rejection::CrossOrigin);
        }
        supplied = true;
    }
    if supplied || site == Some("same-origin") {
        Ok(())
    } else {
        Err(Rejection::Missing)
    }
}

static MUTATIONS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(256);

fn bound_form_body(request: &mut Request) -> Result<(), AppError> {
    let content_type = singleton(request.headers(), "content-type")
        .map_err(|_| AppError::BadRequest("Invalid Content-Type header.".to_owned()))?;
    if content_type.is_some_and(|value| {
        value.split(';').next().is_some_and(|mime| {
            mime.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
    }) {
        let body = std::mem::replace(request.body_mut(), axum::body::Body::empty());
        *request.body_mut() = axum::body::Body::new(http_body_util::Limited::new(body, 256 * 1024));
    }
    Ok(())
}

fn remove_session_cookie(headers: &mut HeaderMap) {
    let retained = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .filter(|part| !part.starts_with("rustpost_session="))
        .collect::<Vec<_>>()
        .join("; ");
    headers.remove(header::COOKIE);
    if !retained.is_empty()
        && let Ok(value) = HeaderValue::from_str(&retained)
    {
        headers.insert(header::COOKIE, value);
    }
}

/// Enforce one provenance policy for every mutation before body extraction.
/// Token-protected sessions may omit privacy-stripped provenance; malformed or
/// explicitly foreign evidence never receives that exception. Login,
/// registration and anonymous posting require positive provenance themselves.
pub async fn request_guard(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let (authority, security) = match destination(&state, &request) {
        Ok(destination) => destination,
        Err(reason) => {
            tracing::warn!(?reason, "request authority rejected");
            return AppError::BadRequest("Invalid or conflicting request host.".to_owned())
                .into_response();
        }
    };
    let https = security.secure_cookie;
    let Ok(session) = crate::auth::checked_session_cookie(request.headers()) else {
        return AppError::Csrf.into_response();
    };
    let mutation = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    let request_media = request.uri().path().starts_with("/uploads/");
    let request_favicon = request.uri().path() == "/favicon.ico";
    let static_path = request.uri().path().starts_with("/assets/")
        || request.uri().path().starts_with("/uploads/");
    let _mutation_permit = if mutation {
        let Ok(permit) = MUTATIONS.try_acquire() else {
            return AppError::Busy.into_response();
        };
        if let Err(error) = bound_form_body(&mut request) {
            return error.into_response();
        }
        Some(permit)
    } else {
        None
    };
    if mutation {
        let allow_missing =
            session.is_some() && !matches!(request.uri().path(), "/login" | "/register");
        match check_provenance(request.headers(), &authority, https) {
            Ok(()) => {}
            Err(Rejection::Missing) if allow_missing => {}
            Err(reason) => {
                tracing::warn!(?reason, "state-changing request provenance rejected");
                return AppError::Origin.into_response();
            }
        }
    }
    let mut clear_expired = false;
    if session.is_some() {
        match crate::auth::current_user(&state.pool, request.headers()).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                if mutation && !matches!(request.uri().path(), "/login" | "/register") {
                    return AppError::Unauthorized.into_response();
                }
                // A stale browser cookie must not make public pages fail or
                // leave users stuck until its independent browser expiry.
                remove_session_cookie(request.headers_mut());
                clear_expired = true;
            }
            Err(error) => return AppError::Anyhow(error).into_response(),
        }
    }
    // HTTP/2 :authority and absolute-form request targets need no Host field.
    if !request.headers().contains_key(header::HOST) {
        let Ok(host) = HeaderValue::from_str(authority.as_str()) else {
            return AppError::Origin.into_response();
        };
        request.headers_mut().insert(header::HOST, host);
    }
    request.extensions_mut().insert(security);
    let mut response = next.run(request).await;
    if clear_expired
        && !response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|value| {
                value
                    .to_str()
                    .is_ok_and(|value| value.starts_with("rustpost_session="))
            })
        && let Ok(cookie) = HeaderValue::from_str(&crate::auth::clear_session_cookie(https))
    {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    if !static_path || mutation || response.headers().contains_key(header::SET_COOKIE) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if request_media || request_favicon {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'; style-src 'unsafe-inline'"),
        );
    }
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
