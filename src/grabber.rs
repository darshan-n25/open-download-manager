//! Site grabber: link discovery and recursive crawling.
//!
//! IDM's grabber is a separate process (`IDMGrHlp.exe`) driven by
//! `HKCU\Software\DownloadManager\GrabberSts\{IncludeFilters,ExcludeFilters,Projects}`
//! and persisted through `sprojects.dat` / `surlexclist.dat`, and it ships a large default
//! exclude list (`defexclist.txt`) so it does not chase tracking pixels. Riptide keeps the
//! same model: include/exclude wildcard filters plus a built-in noise filter.

use std::collections::{HashSet, VecDeque};

use regex::Regex;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::engine::Fetcher;
use crate::error::{Result, RiptideError};
use crate::util::wildcard_to_regex;

/// The default ceiling on pages crawled, so a mis-typed depth cannot run away.
pub const DEFAULT_MAX_PAGES: usize = 200;

/// Media and archive extensions the grabber treats as "worth downloading". This mirrors
/// the extension list IDM exposes in its UI filter.
pub const DOWNLOADABLE_EXTENSIONS: &[&str] = &[
    // video
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "ts", "tsv", "3gp",
    "ogv", "rmvb", "asf", // streaming manifests
    "m3u8", "mpd", "f4m", "ism", // audio
    "mp3", "m4a", "aac", "flac", "wav", "wma", "ogg", "opus",
    // archives and disk images
    "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "img", "dmg",
    // documents and programs
    "pdf", "epub", "mobi", "exe", "msi", "apk", "deb", "rpm", "appimage", "torrent", "srt", "vtt",
];

/// Substrings that almost always mean "advert or analytics beacon", filtered out unless the
/// user explicitly includes them.
const DEFAULT_NOISE: &[&str] = &[
    "doubleclick.net",
    "/pagead/",
    "/ads/",
    "/analytics",
    "google-analytics.com",
    "googletagmanager.com",
    "facebook.com/tr",
    "scorecardresearch.com",
    "/beacon",
    "/tracking",
    "/pixel",
    "1x1.gif",
    "spacer.gif",
    "blank.gif",
];

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct GrabberConfig {
    /// How many link-hops to follow from the start page.
    pub depth: usize,
    pub max_pages: usize,
    /// Stay on the start page's host.
    pub same_host: bool,
    /// Wildcard patterns; a URL must match one to be kept (empty = keep everything).
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Only report download-worthy URLs, skipping HTML pages.
    pub media_only: bool,
}

impl Default for GrabberConfig {
    fn default() -> Self {
        GrabberConfig {
            depth: 1,
            max_pages: DEFAULT_MAX_PAGES,
            same_host: true,
            include: Vec::new(),
            exclude: Vec::new(),
            media_only: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// An HTML/XML document worth crawling further.
    Page,
    /// A media manifest that the HLS/DASH pipeline should handle.
    Manifest,
    /// A plain file to download.
    File,
}

impl TargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetKind::Page => "page",
            TargetKind::Manifest => "manifest",
            TargetKind::File => "file",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrabTarget {
    pub url: String,
    pub kind: TargetKind,
}

/// Lower-case extension of a URL's path, ignoring any query string.
pub fn extension_of(u: &str) -> Option<String> {
    let parsed = Url::parse(u).ok()?;
    let path = parsed.path();
    let last = path.rsplit('/').next()?;
    let ext = last.rsplit_once('.')?.1;
    if ext.is_empty() || ext.len() > 8 {
        None
    } else {
        Some(ext.to_ascii_lowercase())
    }
}

pub fn is_downloadable(u: &str) -> bool {
    match extension_of(u) {
        Some(e) => DOWNLOADABLE_EXTENSIONS.contains(&e.as_str()),
        None => false,
    }
}

pub fn is_manifest(u: &str) -> bool {
    matches!(
        extension_of(u).as_deref(),
        Some("m3u8" | "mpd" | "f4m" | "ism")
    )
}

fn is_noise(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    DEFAULT_NOISE.iter().any(|n| l.contains(n))
}

/// Collect every candidate URL from an HTML (or plain-text) document.
///
/// Deliberately regex-based rather than a full HTML parser: grabbers chase malformed
/// markup constantly and a permissive scan finds URLs a strict parser would drop.
pub fn extract_urls(body: &str, base: &Url) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let push = |raw: &str, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        let raw = raw.trim();
        if raw.is_empty()
            || raw.starts_with('#')
            || raw.starts_with("data:")
            || raw.starts_with("javascript:")
            || raw.starts_with("mailto:")
            || raw.starts_with("blob:")
        {
            return;
        }
        // Strip a fragment: it never affects the fetched bytes.
        let raw = raw.split('#').next().unwrap_or(raw);
        if let Ok(abs) = base.join(raw) {
            if matches!(abs.scheme(), "http" | "https" | "ftp") {
                let s = abs.to_string();
                if seen.insert(s.clone()) {
                    out.push(s);
                }
            }
        }
    };

    if let Ok(re) = Regex::new(
        r#"(?i)(?:href|src|data-src|data-original|poster|content)\s*=\s*["']([^"']+)["']"#,
    ) {
        for c in re.captures_iter(body) {
            push(&c[1], &mut out, &mut seen);
        }
    }

    // CSS url(...) and inline players.
    if let Ok(re) = Regex::new(r#"(?i)url\(\s*['"]?([^'")]+)"#) {
        for c in re.captures_iter(body) {
            push(&c[1], &mut out, &mut seen);
        }
    }

    // Bare absolute URLs in scripts, JSON blobs and manifests.
    if let Ok(re) = Regex::new(r#"https?://[^\s"'<>\\)]+"#) {
        for m in re.find_iter(body) {
            let raw = m.as_str().trim_end_matches([',', ';', '.']);
            push(raw, &mut out, &mut seen);
        }
    }

    // srcset="a.jpg 1x, b.jpg 2x"
    if let Ok(re) = Regex::new(r#"(?i)srcset\s*=\s*["']([^"']+)["']"#) {
        for c in re.captures_iter(body) {
            for part in c[1].split(',') {
                let url = part.split_whitespace().next().unwrap_or("");
                push(url, &mut out, &mut seen);
            }
        }
    }

    out
}

/// Apply the include/exclude filters and the noise filter.
pub fn filter_urls(
    urls: Vec<String>,
    cfg: &GrabberConfig,
    start_host: Option<&str>,
) -> Vec<String> {
    let includes: Vec<Regex> = cfg
        .include
        .iter()
        .filter_map(|p| Regex::new(&wildcard_to_regex(p)).ok())
        .collect();
    let excludes: Vec<Regex> = cfg
        .exclude
        .iter()
        .filter_map(|p| Regex::new(&wildcard_to_regex(p)).ok())
        .collect();

    urls.into_iter()
        .filter(|u| {
            if is_noise(u) {
                return false;
            }
            if let Some(host) = start_host {
                if cfg.same_host {
                    match Url::parse(u)
                        .ok()
                        .and_then(|x| x.host_str().map(|h| h.to_string()))
                    {
                        Some(h) if h == host => {}
                        _ => return false,
                    }
                }
            }
            if excludes.iter().any(|r| r.is_match(u)) {
                return false;
            }
            if !includes.is_empty() && !includes.iter().any(|r| r.is_match(u)) {
                return false;
            }
            true
        })
        .collect()
}

fn classify(u: &str) -> Option<TargetKind> {
    if is_manifest(u) {
        return Some(TargetKind::Manifest);
    }
    if is_downloadable(u) {
        return Some(TargetKind::File);
    }
    let path = Url::parse(u).ok().map(|x| x.path().to_ascii_lowercase())?;
    // A page: a directory-ish path, a classic document extension, or something with no
    // extension at all (plenty of sites serve HTML from `/watch/abc123`).
    let last = path.rsplit('/').next().unwrap_or("");
    let looks_like_page = last.is_empty()
        || last.ends_with(".html")
        || last.ends_with(".htm")
        || last.ends_with(".php")
        || extension_of(u).is_none();
    if looks_like_page {
        Some(TargetKind::Page)
    } else {
        None
    }
}

/// Breadth-first crawl from `start`, returning everything worth downloading.
pub async fn crawl(fetcher: &Fetcher, start: &str, cfg: &GrabberConfig) -> Result<Vec<GrabTarget>> {
    let start_url = Url::parse(start).map_err(RiptideError::Url)?;
    let start_host = start_url.host_str().map(|s| s.to_string());

    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut targets: Vec<GrabTarget> = Vec::new();
    let mut seen_targets: HashSet<String> = HashSet::new();

    queue.push_back((start.to_string(), 0));

    while let Some((url, depth)) = queue.pop_front() {
        if visited.len() >= cfg.max_pages {
            tracing::warn!(limit = cfg.max_pages, "crawl page limit reached");
            break;
        }
        if !visited.insert(url.clone()) {
            continue;
        }

        // The start page itself is always fetched, even in media-only mode.
        let kind = classify(&url);
        if depth > 0 || kind != Some(TargetKind::Page) {
            if let Some(k) = &kind {
                let wanted = match cfg.media_only {
                    true => !matches!(k, TargetKind::Page),
                    false => true,
                };
                if wanted && seen_targets.insert(url.clone()) {
                    targets.push(GrabTarget {
                        url: url.clone(),
                        kind: k.clone(),
                    });
                }
            }
        }

        let is_page = matches!(kind, Some(TargetKind::Page));
        // `depth` counts link hops: depth 0 inspects only the start page, depth 1 also
        // fetches the pages it links to, and so on. The start page itself is always fetched.
        if !is_page || depth > cfg.depth {
            continue;
        }

        let body = match fetcher.to_bytes(&url, 8 << 20).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(url = %url, error = %e, "crawl fetch failed");
                continue;
            }
        };
        let text = String::from_utf8_lossy(&body);
        // Skip binary payloads that slipped through the page heuristic.
        if text.contains('\0') {
            continue;
        }

        let found = extract_urls(&text, &start_url.join(&url).unwrap_or(start_url.clone()));
        let filtered = filter_urls(found, cfg, start_host.as_deref());

        for link in filtered {
            if !visited.contains(&link) {
                queue.push_back((link, depth + 1));
            }
        }
    }

    targets.sort_by(|a, b| a.url.cmp(&b.url));
    targets.dedup_by(|a, b| a.url == b.url);
    Ok(targets)
}

#[cfg(test)]
mod tests {
    // Fixtures here are built field-by-field from `Default::default()`.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    fn base() -> Url {
        Url::parse("https://site.test/dir/page.html").unwrap()
    }

    #[test]
    fn extracts_common_markup() {
        let html = r##"
            <html><head>
              <link href="style.css" rel="stylesheet">
              <base nonsense>
            </head><body>
              <a href="/other/page.html">next</a>
              <a href="https://cdn.other.test/movie.mp4">video</a>
              <video poster="/img/p.jpg" src="/media/clip.mp4"></video>
              <img data-src="img/lazy.png">
              <img srcset="a1.jpg 1x, a2.jpg 2x">
              <a href="#section">skip fragment</a>
              <a href="javascript:void(0)">skip js</a>
            </body></html>
        "##;
        let urls = extract_urls(html, &base());
        assert!(urls.contains(&"https://site.test/dir/style.css".to_string()));
        assert!(urls.contains(&"https://site.test/other/page.html".to_string()));
        assert!(urls.contains(&"https://cdn.other.test/movie.mp4".to_string()));
        assert!(urls.contains(&"https://site.test/media/clip.mp4".to_string()));
        assert!(urls.contains(&"https://site.test/dir/img/lazy.png".to_string()));
        assert!(urls.contains(&"https://site.test/dir/a1.jpg".to_string()));
        assert!(urls.contains(&"https://site.test/dir/a2.jpg".to_string()));
        assert!(!urls.iter().any(|u| u.contains("javascript:")));
        assert!(!urls.iter().any(|u| u.ends_with("#section")));
    }

    #[test]
    fn extracts_urls_from_scripts_and_manifests() {
        let html = r#"<script>var src = "https://cdn.test/v/master.m3u8?";</script>"#;
        let urls = extract_urls(html, &base());
        assert!(urls.iter().any(|u| u.contains("master.m3u8")));
    }

    #[test]
    fn absolute_urls_in_text_are_found() {
        let body = "see https://files.test/archive.zip and https://files.test/a.mpd, ok";
        let urls = extract_urls(body, &base());
        assert!(urls.contains(&"https://files.test/archive.zip".to_string()));
        assert!(urls.contains(&"https://files.test/a.mpd".to_string()));
    }

    #[test]
    fn classification_of_media_pages_and_files() {
        assert_eq!(classify("https://x/a.m3u8"), Some(TargetKind::Manifest));
        assert_eq!(classify("https://x/a.mpd"), Some(TargetKind::Manifest));
        assert_eq!(classify("https://x/a.mp4"), Some(TargetKind::File));
        assert_eq!(classify("https://x/dir/"), Some(TargetKind::Page));
        assert_eq!(classify("https://x/index.html"), Some(TargetKind::Page));
        assert_eq!(classify("https://x/x.php"), Some(TargetKind::Page));
        assert_eq!(classify("https://x/font.woff2"), None);
    }

    #[test]
    fn extension_parsing_ignores_queries() {
        assert_eq!(
            extension_of("https://x/a.MP4?token=1").as_deref(),
            Some("mp4")
        );
        assert_eq!(extension_of("https://x/a"), None);
        assert_eq!(extension_of("https://x/a.verylongextension"), None);
    }

    #[test]
    fn noise_is_filtered_out() {
        let urls = vec![
            "https://site.test/pagead/thing.js".to_string(),
            "https://www.google-analytics.com/collect".to_string(),
            "https://site.test/real.mp4".to_string(),
        ];
        let kept = filter_urls(urls, &GrabberConfig::default(), Some("site.test"));
        assert_eq!(kept, vec!["https://site.test/real.mp4".to_string()]);
    }

    #[test]
    fn same_host_filter_can_be_disabled() {
        let urls = vec![
            "https://other.test/a.mp4".to_string(),
            "https://site.test/b.mp4".to_string(),
        ];
        let mut cfg = GrabberConfig::default();
        cfg.same_host = false;
        assert_eq!(filter_urls(urls.clone(), &cfg, Some("site.test")).len(), 2);

        cfg.same_host = true;
        assert_eq!(
            filter_urls(urls, &cfg, Some("site.test")),
            vec!["https://site.test/b.mp4".to_string()]
        );
    }

    #[test]
    fn include_and_exclude_wildcards() {
        let urls = vec![
            "https://site.test/videos/a.mp4".to_string(),
            "https://site.test/videos/b.mp4".to_string(),
            "https://site.test/other/c.mp4".to_string(),
        ];
        let mut cfg = GrabberConfig::default();
        cfg.include = vec!["*://site.test/videos/*".into()];
        cfg.exclude = vec!["*/b.mp4".into()];
        let kept = filter_urls(urls, &cfg, Some("site.test"));
        assert_eq!(kept, vec!["https://site.test/videos/a.mp4".to_string()]);
    }

    #[test]
    fn downloadable_list_covers_idm_vocabulary() {
        for e in [
            "mp4", "m3u8", "mpd", "ts", "zip", "rar", "torrent", "exe", "apk",
        ] {
            assert!(
                DOWNLOADABLE_EXTENSIONS.contains(&e),
                "{e} should be considered downloadable"
            );
        }
    }
}
