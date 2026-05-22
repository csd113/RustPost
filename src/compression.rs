use std::io::Write as _;

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use flate2::Compression;
use flate2::write::GzEncoder;

const MIN_COMPRESSIBLE_BYTES: usize = 1024;
const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseEncoding {
    Gzip,
}

impl ResponseEncoding {
    const fn header_value(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
        }
    }
}

pub async fn response_compression(mut request: Request, next: Next) -> Response {
    let is_head = request.method() == Method::HEAD;
    let path = request.uri().path().to_owned();
    let encoding = choose_response_encoding(request.headers());

    if is_head {
        *request.method_mut() = Method::GET;
    }

    let response = next.run(request).await;
    let Some(encoding) = encoding else {
        if is_head && should_buffer_head_response(&path, &response) {
            return head_response_with_get_length(response).await;
        }
        return head_safe_response(response, is_head);
    };

    if should_skip_path(&path) || !response_is_compressible(&response) {
        if is_head && should_buffer_head_response(&path, &response) {
            return head_response_with_get_length(response).await;
        }
        return head_safe_response(response, is_head);
    }

    let (mut parts, body) = response.into_parts();
    let body = match to_bytes(body, MAX_BUFFERED_RESPONSE_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(error = %error, "failed to buffer response for compression");
            return Response::from_parts(parts, Body::empty());
        }
    };

    if !meets_minimum_size_threshold(body.len()) {
        set_content_length(&mut parts.headers, body.len());
        return Response::from_parts(parts, body_for_method(body.to_vec(), is_head));
    }

    let compressed = match gzip(&body) {
        Ok(compressed) => compressed,
        Err(error) => {
            tracing::warn!(error = %error, "failed to compress response");
            set_content_length(&mut parts.headers, body.len());
            return Response::from_parts(parts, body_for_method(body.to_vec(), is_head));
        }
    };

    if compressed.len() >= body.len() {
        set_content_length(&mut parts.headers, body.len());
        return Response::from_parts(parts, body_for_method(body.to_vec(), is_head));
    }

    parts.headers.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(encoding.header_value()),
    );
    merge_vary_accept_encoding(&mut parts.headers);
    set_content_length(&mut parts.headers, compressed.len());
    Response::from_parts(parts, body_for_method(compressed, is_head))
}

fn response_is_compressible(response: &Response) -> bool {
    status_allows_body(response.status())
        && !has_content_encoding(response.headers())
        && content_length_allows_buffering(response.headers())
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(is_compressible_content_type)
}

const fn meets_minimum_size_threshold(len: usize) -> bool {
    len >= MIN_COMPRESSIBLE_BYTES
}

fn head_safe_response(response: Response, is_head: bool) -> Response {
    if !is_head {
        return response;
    }
    let (parts, _body) = response.into_parts();
    Response::from_parts(parts, Body::empty())
}

fn should_buffer_head_response(path: &str, response: &Response) -> bool {
    !path.starts_with("/uploads/")
        && !response.headers().contains_key(header::CONTENT_LENGTH)
        && status_allows_body(response.status())
        && content_length_allows_buffering(response.headers())
}

async fn head_response_with_get_length(response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    match to_bytes(body, MAX_BUFFERED_RESPONSE_BYTES).await {
        Ok(body) => {
            set_content_length(&mut parts.headers, body.len());
            Response::from_parts(parts, Body::empty())
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to buffer HEAD response");
            Response::from_parts(parts, Body::empty())
        }
    }
}

fn body_for_method(body: Vec<u8>, is_head: bool) -> Body {
    if is_head {
        Body::empty()
    } else {
        Body::from(body)
    }
}

fn content_length_allows_buffering(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_none_or(|len| len <= MAX_BUFFERED_RESPONSE_BYTES)
}

fn set_content_length(headers: &mut HeaderMap, len: usize) {
    let value = len.to_string();
    match HeaderValue::from_str(&value) {
        Ok(value) => {
            headers.insert(header::CONTENT_LENGTH, value);
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to set compressed content length");
            headers.remove(header::CONTENT_LENGTH);
        }
    }
}

fn status_allows_body(status: StatusCode) -> bool {
    !status.is_informational()
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED
}

fn should_skip_path(path: &str) -> bool {
    path == "/favicon.ico" || path.starts_with("/uploads/")
}

fn has_content_encoding(headers: &HeaderMap) -> bool {
    headers.contains_key(header::CONTENT_ENCODING)
}

fn choose_response_encoding(headers: &HeaderMap) -> Option<ResponseEncoding> {
    let mut gzip = EncodingPreference::absent();
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for accepted in parse_accept_encoding(value) {
            if accepted.name.eq_ignore_ascii_case("gzip") {
                gzip.merge(accepted.q);
            }
        }
    }

    if gzip.is_accepted() {
        Some(ResponseEncoding::Gzip)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct EncodingPreference {
    q: f32,
    present: bool,
}

impl EncodingPreference {
    const fn absent() -> Self {
        Self {
            q: 0.0,
            present: false,
        }
    }

    fn merge(&mut self, q: f32) {
        self.present = true;
        self.q = self.q.max(q);
    }

    const fn is_accepted(self) -> bool {
        self.present && self.q > 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AcceptedEncoding<'a> {
    name: &'a str,
    q: f32,
}

fn parse_accept_encoding(value: &str) -> impl Iterator<Item = AcceptedEncoding<'_>> {
    value.split(',').filter_map(|part| {
        let mut pieces = part.split(';');
        let name = pieces.next()?.trim();
        if name.is_empty() {
            return None;
        }
        let mut q = 1.0_f32;
        for parameter in pieces {
            let Some((key, value)) = parameter.trim().split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("q") {
                q = value.trim().parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0);
            }
        }
        Some(AcceptedEncoding { name, q })
    })
}

fn is_compressible_content_type(value: &HeaderValue) -> bool {
    let Ok(value) = value.to_str() else {
        return false;
    };
    let media_type = value
        .split_once(';')
        .map_or(value, |(media_type, _parameters)| media_type)
        .trim()
        .to_ascii_lowercase();

    media_type.starts_with("text/")
        || matches!(
            media_type.as_str(),
            "application/javascript"
                | "application/json"
                | "application/manifest+json"
                | "application/rss+xml"
                | "application/xhtml+xml"
                | "application/xml"
                | "image/svg+xml"
        )
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml")
}

fn gzip(body: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body)?;
    encoder.finish()
}

fn merge_vary_accept_encoding(headers: &mut HeaderMap) {
    let mut values = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();

    if values.iter().any(|value| {
        vary_contains_token(value, "*") || vary_contains_token(value, "accept-encoding")
    }) {
        return;
    }

    values.push("Accept-Encoding");
    let merged = values.join(", ");
    match HeaderValue::from_str(&merged) {
        Ok(value) => {
            headers.insert(header::VARY, value);
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to merge Vary header");
        }
    }
}

fn vary_contains_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_encoding_parser_reads_q_values() {
        let encodings = parse_accept_encoding("br, gzip;q=0.7, deflate; q=0").collect::<Vec<_>>();

        assert_eq!(
            encodings,
            vec![
                AcceptedEncoding { name: "br", q: 1.0 },
                AcceptedEncoding {
                    name: "gzip",
                    q: 0.7
                },
                AcceptedEncoding {
                    name: "deflate",
                    q: 0.0
                },
            ]
        );
    }

    #[test]
    fn chooses_gzip_only_when_explicitly_accepted() {
        let mut headers = HeaderMap::new();
        assert_eq!(choose_response_encoding(&headers), None);

        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        assert_eq!(choose_response_encoding(&headers), None);

        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=0"),
        );
        assert_eq!(choose_response_encoding(&headers), None);

        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip;q=0.5"),
        );
        assert_eq!(
            choose_response_encoding(&headers),
            Some(ResponseEncoding::Gzip)
        );
    }

    #[test]
    fn content_type_decision_accepts_text_candidates() {
        for value in [
            "text/html; charset=utf-8",
            "text/css",
            "application/javascript; charset=utf-8",
            "application/json",
            "application/activity+json",
            "application/xml",
            "image/svg+xml",
        ] {
            assert!(is_compressible_content_type(&HeaderValue::from_static(
                value
            )));
        }
    }

    #[test]
    fn content_type_decision_rejects_binary_media() {
        for value in [
            "image/png",
            "image/webp",
            "video/mp4",
            "audio/mpeg",
            "application/pdf",
            "application/zip",
            "application/octet-stream",
        ] {
            assert!(!is_compressible_content_type(&HeaderValue::from_static(
                value
            )));
        }
    }

    #[test]
    fn already_encoded_responses_are_not_candidates() {
        let mut response = Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from("<html></html>"))
            .expect("response");
        assert!(response_is_compressible(&response));

        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
        assert!(!response_is_compressible(&response));
    }

    #[test]
    fn content_length_over_buffer_limit_is_not_candidate() {
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .header(
                header::CONTENT_LENGTH,
                (MAX_BUFFERED_RESPONSE_BYTES + 1).to_string(),
            )
            .body(Body::empty())
            .expect("response");

        assert!(!response_is_compressible(&response));
    }

    #[test]
    fn size_threshold_skips_tiny_responses() {
        assert!(!meets_minimum_size_threshold(MIN_COMPRESSIBLE_BYTES - 1));
        assert!(meets_minimum_size_threshold(MIN_COMPRESSIBLE_BYTES));
    }

    #[test]
    fn vary_accept_encoding_is_merged_without_clobbering() {
        let mut headers = HeaderMap::new();
        headers.insert(header::VARY, HeaderValue::from_static("Cookie"));

        merge_vary_accept_encoding(&mut headers);
        assert_eq!(headers[header::VARY], "Cookie, Accept-Encoding");

        merge_vary_accept_encoding(&mut headers);
        assert_eq!(headers[header::VARY], "Cookie, Accept-Encoding");
    }
}
