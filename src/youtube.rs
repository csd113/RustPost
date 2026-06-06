use std::{collections::BTreeSet, sync::OnceLock, time::Duration};

use axum::http::Uri;
use futures_util::future::join_all;
use serde::Deserialize;

#[cfg(test)]
use std::{future::Future, sync::Arc};

pub const FALLBACK_TITLE: &str = "YouTube video";
pub const MAX_EMBEDS_PER_POST: usize = 3;

const VIDEO_ID_LEN: usize = 11;
const MAX_TITLE_CHARS: usize = 180;
const MAX_OEMBED_BYTES: u64 = 16 * 1024;
const OEMBED_USER_AGENT: &str = concat!(
    "RustPost/",
    env!("CARGO_PKG_VERSION"),
    " YouTube metadata fetch"
);
const RESOLVE_TIMEOUT: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const READ_TIMEOUT: Duration = Duration::from_millis(700);
const FETCH_TIMEOUT: Duration = Duration::from_millis(1_300);

#[cfg(test)]
type TestOembedFetcher = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[cfg(test)]
tokio::task_local! {
    static TEST_OEMBED_FETCHER: TestOembedFetcher;
}

#[cfg(test)]
pub async fn with_test_oembed_fetcher<F, Fut, R>(fetcher: F, future: Fut) -> R
where
    F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    Fut: Future<Output = R>,
{
    TEST_OEMBED_FETCHER.scope(Arc::new(fetcher), future).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoutubeEmbed {
    pub video_id: String,
    pub source_url: String,
    pub canonical_url: String,
    pub title: Option<String>,
    pub thumbnail_url: String,
    pub embed_url: String,
}

impl YoutubeEmbed {
    fn new(video_id: String, source_url: String, title: Option<String>) -> Self {
        let canonical_url = canonical_watch_url(&video_id);
        let thumbnail_url = thumbnail_url(&video_id);
        let embed_url = embed_url(&video_id);
        Self {
            video_id,
            source_url,
            canonical_url,
            title,
            thumbnail_url,
            embed_url,
        }
    }

    pub fn display_title(&self) -> &str {
        self.title.as_deref().unwrap_or(FALLBACK_TITLE)
    }
}

#[derive(Debug, Deserialize)]
struct OembedResponse {
    title: Option<String>,
}

pub fn embeds_for_text(text: &str) -> Vec<YoutubeEmbed> {
    let mut embeds = Vec::new();
    let mut seen_video_ids = BTreeSet::new();
    for word in text.split_whitespace() {
        if embeds.len() >= MAX_EMBEDS_PER_POST {
            break;
        }
        let Some(candidate) = youtube_candidate_from_word(word) else {
            continue;
        };
        let Some(embed) = embed_from_url(candidate) else {
            continue;
        };
        if seen_video_ids.insert(embed.video_id.clone()) {
            embeds.push(embed);
        }
    }
    embeds
}

pub async fn metadata_for_text(text: &str) -> Vec<YoutubeEmbed> {
    metadata_for_embeds(embeds_for_text(text)).await
}

pub async fn metadata_for_embeds(embeds: Vec<YoutubeEmbed>) -> Vec<YoutubeEmbed> {
    join_all(embeds.into_iter().map(fetch_embed_title)).await
}

pub fn embed_from_stored(video_id: &str, title: Option<String>) -> Option<YoutubeEmbed> {
    let video_id = valid_youtube_video_id(video_id)?;
    Some(YoutubeEmbed::new(
        video_id.clone(),
        canonical_watch_url(&video_id),
        title.and_then(|value| sanitize_title(&value)),
    ))
}

async fn fetch_embed_title(mut embed: YoutubeEmbed) -> YoutubeEmbed {
    if embed.title.is_none()
        && let Some(title) = fetch_oembed_title(&embed.video_id).await
    {
        embed.title = Some(title);
    }
    embed
}

async fn fetch_oembed_title(video_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Ok(title) = TEST_OEMBED_FETCHER.try_with(|fetcher| fetcher(video_id)) {
        let title = title?;
        return sanitize_title(&title);
    }

    let video_id = video_id.to_owned();
    tokio::task::spawn_blocking(move || fetch_oembed_title_blocking(&video_id))
        .await
        .ok()
        .flatten()
}

fn fetch_oembed_title_blocking(video_id: &str) -> Option<String> {
    let oembed_url = format!(
        "https://www.youtube.com/oembed?format=json&url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3D{video_id}"
    );
    let mut response = oembed_agent().get(&oembed_url).call().ok()?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_OEMBED_BYTES)
        .read_to_string()
        .ok()?;
    parse_oembed_title(&body)
}

fn oembed_agent() -> ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::Agent::config_builder()
                .user_agent(OEMBED_USER_AGENT)
                .accept("application/json")
                .max_response_header_size(8 * 1024)
                .timeout_global(Some(FETCH_TIMEOUT))
                .timeout_resolve(Some(RESOLVE_TIMEOUT))
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_recv_response(Some(READ_TIMEOUT))
                .timeout_recv_body(Some(READ_TIMEOUT))
                .build()
                .into()
        })
        .clone()
}

fn parse_oembed_title(body: &str) -> Option<String> {
    let response = serde_json::from_str::<OembedResponse>(body).ok()?;
    let title = response.title?;
    sanitize_title(&title)
}

fn embed_from_url(candidate: &str) -> Option<YoutubeEmbed> {
    let href = youtube_absolute_http_url(candidate)?;
    let uri = href.parse::<Uri>().ok()?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return None;
    }
    let host = uri.host()?.to_ascii_lowercase();
    let video_id = youtube_video_id_from_parts(&host, uri.path(), uri.query())?;
    Some(YoutubeEmbed::new(video_id, href, None))
}

fn youtube_candidate_from_word(word: &str) -> Option<&str> {
    let candidate = word
        .trim_start_matches(is_url_leading_trim_char)
        .trim_end_matches(is_url_trailing_trim_char);
    (!candidate.is_empty()).then_some(candidate)
}

fn is_url_leading_trim_char(ch: char) -> bool {
    matches!(ch, '<' | '(' | '[' | '{' | '"' | '\'')
}

fn is_url_trailing_trim_char(ch: char) -> bool {
    matches!(
        ch,
        '.' | ',' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '>' | '"' | '\''
    )
}

fn youtube_absolute_http_url(candidate: &str) -> Option<String> {
    if has_http_scheme(candidate) {
        return Some(candidate.to_owned());
    }
    let (host, _path) = candidate.split_once('/')?;
    is_supported_youtube_host(host).then(|| format!("https://{candidate}"))
}

fn has_http_scheme(candidate: &str) -> bool {
    candidate
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || candidate
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn youtube_video_id_from_parts(host: &str, path: &str, query: Option<&str>) -> Option<String> {
    if is_youtu_be_host(host) {
        return single_path_segment(path).and_then(valid_youtube_video_id);
    }
    if is_youtube_nocookie_host(host) {
        return path_segment_after_prefix(path, "/embed/").and_then(valid_youtube_video_id);
    }
    if !is_youtube_host(host) {
        return None;
    }
    if path == "/watch" {
        return query.and_then(youtube_video_id_from_query);
    }
    path_segment_after_prefix(path, "/shorts/")
        .or_else(|| path_segment_after_prefix(path, "/embed/"))
        .and_then(valid_youtube_video_id)
}

fn youtube_video_id_from_query(query: &str) -> Option<String> {
    let mut video_id = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "v" {
            continue;
        }
        let valid = valid_youtube_video_id(value)?;
        if video_id.replace(valid).is_some() {
            return None;
        }
    }
    video_id
}

fn single_path_segment(path: &str) -> Option<&str> {
    let value = path.strip_prefix('/')?;
    (!value.is_empty() && !value.contains('/')).then_some(value)
}

fn path_segment_after_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let value = path.strip_prefix(prefix)?;
    (!value.is_empty() && !value.contains('/')).then_some(value)
}

fn valid_youtube_video_id(value: &str) -> Option<String> {
    (value.len() == VIDEO_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| value.to_owned())
}

fn is_supported_youtube_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    is_youtube_host(&host) || is_youtu_be_host(&host) || is_youtube_nocookie_host(&host)
}

fn is_youtube_host(host: &str) -> bool {
    matches!(host, "youtube.com" | "www.youtube.com" | "m.youtube.com")
}

fn is_youtu_be_host(host: &str) -> bool {
    host == "youtu.be"
}

fn is_youtube_nocookie_host(host: &str) -> bool {
    matches!(host, "youtube-nocookie.com" | "www.youtube-nocookie.com")
}

fn sanitize_title(value: &str) -> Option<String> {
    let mut title = String::new();
    for character in value.trim().chars().take(MAX_TITLE_CHARS) {
        if character.is_control() {
            if matches!(character, '\n' | '\r' | '\t') {
                title.push(' ');
            }
            continue;
        }
        title.push(character);
    }
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

fn canonical_watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

fn thumbnail_url(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")
}

fn embed_url(video_id: &str) -> String {
    format!("https://www.youtube-nocookie.com/embed/{video_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_extraction_supports_common_url_shapes() {
        let cases = [
            ("https://www.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ"),
            (
                "https://youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
                "dQw4w9WgXcQ",
            ),
            ("https://youtu.be/dQw4w9WgXcQ?t=12", "dQw4w9WgXcQ"),
            ("https://www.youtube.com/shorts/dQw4w9WgXcQ", "dQw4w9WgXcQ"),
            ("https://www.youtube.com/embed/dQw4w9WgXcQ", "dQw4w9WgXcQ"),
            (
                "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
                "dQw4w9WgXcQ",
            ),
            ("youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ"),
        ];

        for (url, expected) in cases {
            let preview = embed_from_url(url).expect("valid YouTube URL");

            assert_eq!(preview.video_id, expected);
            assert_eq!(preview.canonical_url, canonical_watch_url(expected));
        }
    }

    #[test]
    fn video_id_extraction_rejects_invalid_and_spoofed_urls() {
        let invalid = [
            "https://youtube.com.evil/watch?v=dQw4w9WgXcQ",
            "https://evil.example/youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com@evil.example/watch?v=dQw4w9WgXcQ",
            "javascript:https://youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXc",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ%2F",
            "https://youtu.be/dQw4w9WgXcQ/extra",
            "https://www.youtube.com/shorts/../dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ/../x",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&v=aaaaaaaaaaa",
        ];

        for url in invalid {
            assert!(embed_from_url(url).is_none(), "accepted invalid URL: {url}");
        }
    }

    #[test]
    fn text_extraction_deduplicates_and_caps_embeds() {
        let embeds = embeds_for_text(
            "https://youtu.be/dQw4w9WgXcQ https://youtube.com/watch?v=dQw4w9WgXcQ https://youtube.com/shorts/aaaaaaaaaaa https://youtube.com/embed/bbbbbbbbbbb https://youtube.com/watch?v=ccccccccccc",
        );

        assert_eq!(embeds.len(), MAX_EMBEDS_PER_POST);
        assert_eq!(embeds[0].video_id, "dQw4w9WgXcQ");
        assert_eq!(embeds[1].video_id, "aaaaaaaaaaa");
        assert_eq!(embeds[2].video_id, "bbbbbbbbbbb");
    }

    #[test]
    fn stored_embeds_validate_id_and_sanitize_title() {
        let embed = embed_from_stored("dQw4w9WgXcQ", Some("  A title\nwith\tspacing  ".to_owned()))
            .expect("valid stored embed");

        assert_eq!(embed.display_title(), "A title with spacing");
        assert_eq!(
            embed.embed_url,
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ"
        );
        assert!(embed_from_stored("bad", Some("title".to_owned())).is_none());
    }

    #[tokio::test]
    async fn metadata_fetch_uses_sanitized_oembed_title_when_available() {
        let embeds = with_test_oembed_fetcher(
            |video_id| {
                assert_eq!(video_id, "dQw4w9WgXcQ");
                Some("  A fetched\n<title>  ".to_owned())
            },
            metadata_for_text("watch https://youtu.be/dQw4w9WgXcQ"),
        )
        .await;

        assert_eq!(embeds.len(), 1);
        assert_eq!(embeds[0].title.as_deref(), Some("A fetched <title>"));
        assert_eq!(embeds[0].display_title(), "A fetched <title>");
    }

    #[tokio::test]
    async fn metadata_fetch_keeps_fallback_title_when_oembed_fails() {
        let embeds = with_test_oembed_fetcher(
            |_video_id| None,
            metadata_for_text("https://youtu.be/dQw4w9WgXcQ"),
        )
        .await;

        assert_eq!(embeds.len(), 1);
        assert_eq!(embeds[0].title, None);
        assert_eq!(embeds[0].display_title(), FALLBACK_TITLE);
    }

    #[test]
    fn oembed_title_parsing_rejects_invalid_or_empty_title() {
        assert_eq!(
            parse_oembed_title(r#"{"title":"  Valid\tTitle  "}"#),
            Some("Valid Title".to_owned())
        );
        assert_eq!(parse_oembed_title(r#"{"title":" \n\t "}"#), None);
        assert_eq!(parse_oembed_title(r#"{"title":42}"#), None);
        assert_eq!(parse_oembed_title("not json"), None);
    }
}
