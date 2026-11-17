use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{MimikyuError, Result};

/// Normalized network resource type across Chromium CDP and Firefox BiDi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    Document,
    Stylesheet,
    Image,
    Media,
    Font,
    Script,
    TextTrack,
    Xhr,
    Fetch,
    EventSource,
    WebSocket,
    Manifest,
    Other,
}

impl ResourceType {
    /// Map CDP `Network.ResourceType` string into `ResourceType`.
    pub fn from_cdp(s: &str) -> Self {
        match s {
            "Document" => Self::Document,
            "Stylesheet" => Self::Stylesheet,
            "Image" => Self::Image,
            "Media" => Self::Media,
            "Font" => Self::Font,
            "Script" => Self::Script,
            "TextTrack" => Self::TextTrack,
            "XHR" => Self::Xhr,
            "Fetch" => Self::Fetch,
            "EventSource" => Self::EventSource,
            "WebSocket" => Self::WebSocket,
            "Manifest" => Self::Manifest,
            _ => Self::Other,
        }
    }

    /// Map W3C WebDriver BiDi `request.destination` (and optional `initiatorType`) into `ResourceType`.
    pub fn from_bidi(destination: &str, initiator_type: Option<&str>) -> Self {
        Self::from_bidi_event(destination, initiator_type, false)
    }

    pub fn from_bidi_event(
        destination: &str,
        initiator_type: Option<&str>,
        is_navigation: bool,
    ) -> Self {
        match destination.to_ascii_lowercase().as_str() {
            "document" | "iframe" | "frame" => Self::Document,
            "style" => Self::Stylesheet,
            "image" => Self::Image,
            "audio" | "video" => Self::Media,
            "track" => Self::TextTrack,
            "font" => Self::Font,
            "script" | "worker" | "sharedworker" | "serviceworker" => Self::Script,
            "manifest" => Self::Manifest,
            "" => {
                if is_navigation {
                    Self::Document
                } else {
                    match initiator_type.map(|s| s.to_ascii_lowercase()).as_deref() {
                        Some("xmlhttprequest") => Self::Xhr,
                        Some("fetch") | Some("script") | Some("other") | None => Self::Fetch,
                        _ => Self::Other,
                    }
                }
            }
            _ => Self::Other,
        }
    }

    /// Convert to CDP `Network.ResourceType` string for `Fetch.enable` `RequestPattern`.
    pub fn to_cdp_str(&self) -> &'static str {
        match self {
            Self::Document => "Document",
            Self::Stylesheet => "Stylesheet",
            Self::Image => "Image",
            Self::Media => "Media",
            Self::Font => "Font",
            Self::Script => "Script",
            Self::TextTrack => "TextTrack",
            Self::Xhr => "XHR",
            Self::Fetch => "Fetch",
            Self::EventSource => "EventSource",
            Self::WebSocket => "WebSocket",
            Self::Manifest => "Manifest",
            Self::Other => "Other",
        }
    }
}

/// Compiled URL pattern supporting `exact:`, `prefix:`, `substring:`, `regex:`, and `*`/`?` globs.
#[derive(Debug, Clone)]
pub enum UrlPattern {
    Exact(String),
    Prefix(String),
    Substring(String),
    Glob(String),
    Regex(Regex),
}

impl UrlPattern {
    pub fn parse(raw: &str) -> Result<Self> {
        if let Some(pat) = raw.strip_prefix("regex:") {
            let re = Regex::new(pat).map_err(|e| {
                MimikyuError::InvalidConfig(format!("invalid URL regex pattern '{pat}': {e}"))
            })?;
            return Ok(Self::Regex(re));
        }
        if let Some(s) = raw.strip_prefix("exact:") {
            return Ok(Self::Exact(s.to_string()));
        }
        if let Some(s) = raw.strip_prefix("prefix:") {
            return Ok(Self::Prefix(s.to_string()));
        }
        if let Some(s) = raw.strip_prefix("substring:") {
            return Ok(Self::Substring(s.to_string()));
        }
        if raw.contains('*') || raw.contains('?') {
            return Ok(Self::Glob(raw.to_string()));
        }
        Ok(Self::Substring(raw.to_string()))
    }

    pub fn matches(&self, url: &str) -> bool {
        match self {
            Self::Exact(s) => url == s,
            Self::Prefix(s) => url.starts_with(s),
            Self::Substring(s) => url.contains(s),
            Self::Glob(pat) => wildcard_match(pat.as_bytes(), url.as_bytes()),
            Self::Regex(re) => re.is_match(url),
        }
    }

    /// Convert to a CDP `Network.setBlockedURLs` wildcard glob when representable without exact/regex.
    pub fn to_cdp_blocked_url_glob(&self) -> Option<String> {
        match self {
            Self::Glob(pat) => Some(pat.clone()),
            Self::Substring(s) => Some(format!("*{s}*")),
            Self::Prefix(s) => Some(format!("{s}*")),
            Self::Exact(_) | Self::Regex(_) => None,
        }
    }
}

/// Declarative blocking policy for resource categories and URL patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockPolicy {
    pub block_images: bool,
    pub block_media: bool,
    pub block_fonts: bool,
    #[serde(default)]
    pub blocked_urls: Vec<String>,
}

/// Compiled resource and URL blocker.
#[derive(Debug, Clone, Default)]
pub struct ResourceBlocker {
    policy: BlockPolicy,
    compiled_urls: Vec<UrlPattern>,
}

impl ResourceBlocker {
    pub fn new(policy: BlockPolicy) -> Result<Self> {
        let mut compiled_urls = Vec::with_capacity(policy.blocked_urls.len());
        for raw in &policy.blocked_urls {
            compiled_urls.push(UrlPattern::parse(raw)?);
        }
        Ok(Self {
            policy,
            compiled_urls,
        })
    }

    pub fn policy(&self) -> &BlockPolicy {
        &self.policy
    }

    pub fn set_blocked_urls(&mut self, patterns: Vec<String>) -> Result<()> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for raw in &patterns {
            compiled.push(UrlPattern::parse(raw)?);
        }
        self.policy.blocked_urls = patterns;
        self.compiled_urls = compiled;
        Ok(())
    }

    pub fn set_categories(&mut self, block_images: bool, block_media: bool, block_fonts: bool) {
        self.policy.block_images = block_images;
        self.policy.block_media = block_media;
        self.policy.block_fonts = block_fonts;
    }

    pub fn has_category_blocks(&self) -> bool {
        self.policy.block_images || self.policy.block_media || self.policy.block_fonts
    }

    pub fn has_any_blocks(&self) -> bool {
        self.has_category_blocks() || !self.compiled_urls.is_empty()
    }

    pub fn should_block_url(&self, url: &str) -> bool {
        self.compiled_urls.iter().any(|p| p.matches(url))
    }

    pub fn should_block_resource_type(&self, rtype: ResourceType, url: &str) -> bool {
        match rtype {
            ResourceType::Image => self.policy.block_images,
            ResourceType::Media | ResourceType::TextTrack => self.policy.block_media,
            ResourceType::Font => self.policy.block_fonts,
            ResourceType::Other => {
                let ext = url_extension(url);
                match ext.as_deref() {
                    Some(e) if self.policy.block_images && is_image_ext(e) => true,
                    Some(e) if self.policy.block_media && is_media_ext(e) => true,
                    Some(e) if self.policy.block_fonts && is_font_ext(e) => true,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    pub fn blocked_cdp_resource_types(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.policy.block_images {
            out.push(ResourceType::Image.to_cdp_str());
        }
        if self.policy.block_media {
            out.push(ResourceType::Media.to_cdp_str());
        }
        if self.policy.block_fonts {
            out.push(ResourceType::Font.to_cdp_str());
        }
        out
    }

    pub fn cdp_blocked_url_globs(&self) -> Vec<String> {
        self.compiled_urls
            .iter()
            .filter_map(UrlPattern::to_cdp_blocked_url_glob)
            .collect()
    }

    pub fn has_non_cdp_url_patterns(&self) -> bool {
        self.compiled_urls
            .iter()
            .any(|p| p.to_cdp_blocked_url_glob().is_none())
    }
}

fn url_extension(url: &str) -> Option<String> {
    let without_frag = url.split('#').next().unwrap_or(url);
    let without_query = without_frag.split('?').next().unwrap_or(without_frag);
    let path_str = if let Ok(parsed) = url::Url::parse(without_query) {
        parsed.path().to_string()
    } else {
        without_query.to_string()
    };
    Path::new(&path_str)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
}

fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "avif"
    )
}

fn is_media_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp4" | "webm" | "ogg" | "mp3" | "wav" | "m3u8" | "mpd" | "m4a" | "flac"
    )
}

fn is_font_ext(ext: &str) -> bool {
    matches!(ext, "woff" | "woff2" | "ttf" | "otf" | "eot")
}

fn wildcard_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while t < text.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star_p = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_pattern_supports_all_modes_and_validates_regex() {
        let exact = UrlPattern::parse("exact:http://127.0.0.1/api").unwrap();
        assert!(exact.matches("http://127.0.0.1/api"));
        assert!(!exact.matches("http://127.0.0.1/api/v2"));
        assert_eq!(exact.to_cdp_blocked_url_glob(), None);

        let prefix = UrlPattern::parse("prefix:http://127.0.0.1/assets/").unwrap();
        assert!(prefix.matches("http://127.0.0.1/assets/a.png"));
        assert!(!prefix.matches("http://127.0.0.1/api/a.png"));
        assert_eq!(
            prefix.to_cdp_blocked_url_glob().as_deref(),
            Some("http://127.0.0.1/assets/*")
        );

        let sub = UrlPattern::parse("example.com").unwrap();
        assert!(sub.matches("https://example.com/x"));
        assert_eq!(sub.to_cdp_blocked_url_glob().as_deref(), Some("*example.com*"));

        let glob = UrlPattern::parse("*/api/mock?x=*").unwrap();
        assert!(glob.matches("http://127.0.0.1/api/mock?x=1"));
        assert!(!glob.matches("http://127.0.0.1/api/real"));

        let re = UrlPattern::parse(r"regex:^http://127\.0\.0\.1(:\d+)?/api/v[12]/item$").unwrap();
        assert!(re.matches("http://127.0.0.1:8080/api/v2/item"));
        assert!(!re.matches("http://127.0.0.1:8080/api/v3/item"));
        assert_eq!(re.to_cdp_blocked_url_glob(), None);

        assert!(UrlPattern::parse("regex:[unclosed").is_err());
    }

    #[test]
    fn resource_blocker_handles_categories_and_other_fallback() {
        let blocker = ResourceBlocker::new(BlockPolicy {
            block_images: true,
            block_media: true,
            block_fonts: true,
            blocked_urls: vec!["exact:http://127.0.0.1/blocked".to_string()],
        })
        .unwrap();

        assert!(blocker.should_block_url("http://127.0.0.1/blocked"));
        assert!(!blocker.should_block_url("http://127.0.0.1/blocked/2"));

        // Extensionless image/media/font blocked via ResourceType
        assert!(blocker.should_block_resource_type(
            ResourceType::Image,
            "http://127.0.0.1/avatar?id=99"
        ));
        assert!(blocker.should_block_resource_type(
            ResourceType::Media,
            "http://127.0.0.1/tone?id=1"
        ));
        assert!(blocker.should_block_resource_type(
            ResourceType::Font,
            "http://127.0.0.1/font?id=2"
        ));

        // Document is never blocked by category filter even if URL ends with .png
        assert!(!blocker.should_block_resource_type(
            ResourceType::Document,
            "http://127.0.0.1/view.png"
        ));

        // `Other` falls back to extension
        assert!(blocker.should_block_resource_type(
            ResourceType::Other,
            "http://127.0.0.1/assets/pic.png?v=1"
        ));
        assert!(blocker.should_block_resource_type(
            ResourceType::Other,
            "http://127.0.0.1/assets/test.woff2#hash"
        ));
        assert!(!blocker.should_block_resource_type(
            ResourceType::Other,
            "http://127.0.0.1/api/data"
        ));
    }

    #[test]
    fn resource_type_parses_cdp_and_bidi_destinations() {
        assert_eq!(ResourceType::from_cdp("Image"), ResourceType::Image);
        assert_eq!(ResourceType::from_cdp("XHR"), ResourceType::Xhr);
        assert_eq!(
            ResourceType::from_bidi("image", None),
            ResourceType::Image
        );
        assert_eq!(
            ResourceType::from_bidi("video", None),
            ResourceType::Media
        );
        assert_eq!(
            ResourceType::from_bidi("audio", None),
            ResourceType::Media
        );
        assert_eq!(
            ResourceType::from_bidi("font", None),
            ResourceType::Font
        );
        assert_eq!(
            ResourceType::from_bidi("", Some("fetch")),
            ResourceType::Fetch
        );
    }
}
