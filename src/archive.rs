//! Offline site archiving: fetch a page **and everything it needs to render**, then rewrite
//! the markup so the copy works from disk.
//!
//! IDM's grabber offers this next to plain link harvesting ("Complete start page with all
//! pictures, styles, and scripts for offline browsing", "Convert the links in downloaded
//! html files to the local files for offline browsing"). Riptide's grabber deliberately only
//! *found* targets; this module is the other half.
//!
//! Three decisions shape everything here:
//!
//! 1. **Every URL gets a place on disk that is decided by the URL alone.** See
//!    [`local_path`]. No counters, no visit order, no collisions to arbitrate: the same page
//!    archived twice lands on the same files, which is what makes a scheduled archive a
//!    synchronization rather than a pile of copies.
//! 2. **Rewriting leaves anything it cannot place pointing at the live site.** A link the
//!    user's filters excluded, or a target that 404'd, is rewritten to an absolute URL
//!    rather than to a local path that does not exist. An archive that silently dead-links
//!    is worse than one that says "this part is not here", and [`ArchiveReport::remote_links`]
//!    counts them so the gap is visible.
//! 3. **Documents and stylesheets are re-read on every run; everything else is asked about
//!    first.** HTML and CSS are the maps to everything else — skipping them on a `304` would
//!    mean never noticing that an image their text points at has changed. They are small.
//!    Images, fonts and scripts are conditional, so an unchanged archive moves no bodies.
//!
//! Like the rest of Riptide, every byte goes through [`crate::engine::Fetcher`], so an
//! archive is subject to the same global and per-file ceilings, the same per-server
//! connection ceiling and the same download quota as an interactive download.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use url::Url;

use crate::engine::{Conditional, Fetcher};
use crate::error::{Result, RiptideError};
use crate::grabber;
use crate::grabber::GrabberConfig;
use crate::project::Validator;
use crate::util;

/// The reserved top-level directory holding copies of hosts other than the start host.
const HOST_DIR: &str = "_hosts";

/// The reserved top-level directory holding the archive's own bookkeeping.
pub const BOOKKEEPING_DIR: &str = "_riptide";

/// Where an archive remembers what the server said about each asset, so a later run can ask
/// instead of pulling the body again.
///
/// It lives *inside* the archive rather than in the process's state directory: the mirror is
/// then self-contained, and moving or copying it does not make the next run re-fetch
/// everything. A project keeps the same information in its own document instead.
pub const MANIFEST_FILE: &str = "validators.json";

/// Extensions whose bodies are markup that gets rewritten and links that get followed.
const DOCUMENT_EXTENSIONS: &[&str] = &[
    "html", "htm", "xhtml", "shtml", "dhtml", "php", "asp", "aspx", "jsp", "cgi", "cfm",
];

/// Extensions whose bodies are stylesheets: rewritten, and followed for `url(...)` targets.
const STYLESHEET_EXTENSIONS: &[&str] = &["css"];

/// Everything an offline copy needs in order to *render* a page.
///
/// Deliberately narrower than [`grabber::DOWNLOADABLE_EXTENSIONS`]. Video, archives and
/// installers are things to download, not things a page needs to look right; an archive that
/// drags them in stops being a copy of a document and becomes a copy of a server.
const ASSET_EXTENSIONS: &[&str] = &[
    // scripts and styles
    "js",
    "mjs",
    "cjs",
    "css", // images
    "png",
    "jpg",
    "jpeg",
    "jpe",
    "jfif",
    "gif",
    "webp",
    "avif",
    "svg",
    "ico",
    "bmp",
    "tif",
    "tiff",
    "cur", // fonts
    "woff",
    "woff2",
    "ttf",
    "otf",
    "eot", // small data a page loads
    "json",
    "xml",
    "txt",
    "csv",
    "webmanifest",
    "manifest",
    "appcache",
    "map",
    "wasm",
    "vtt",
    "srt",
];

/// Default ceiling on how many assets one archive run holds.
///
/// The same number a `site` project uses for `--max-downloads`, because from the outside
/// "how much does one run fetch" is one question.
pub const DEFAULT_MAX_ASSETS: usize = 500;

/// How an archive run should behave.
#[derive(Clone, Debug)]
pub struct ArchiveConfig {
    /// All the crawl knobs: `depth` is measured in *page* hops, `max_pages` caps documents,
    /// `same_host` and the include/exclude filters are shared with the plain grabber.
    ///
    /// `media_only` is ignored: an archive always keeps documents, because they are what a
    /// reader opens.
    pub grab: GrabberConfig,
    /// Cap on buffering one document or stylesheet for rewriting.
    pub max_doc_bytes: u64,
    /// Cap on a single asset.
    pub max_asset_bytes: u64,
    /// Cap on how many assets one run fetches, so a page listing thousands of images cannot
    /// turn a scheduled run into a stampede.
    pub max_assets: usize,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        ArchiveConfig {
            grab: GrabberConfig {
                depth: 1,
                media_only: false,
                ..GrabberConfig::default()
            },
            max_doc_bytes: 8 << 20,
            max_asset_bytes: 64 << 20,
            max_assets: DEFAULT_MAX_ASSETS,
        }
    }
}

/// What one archive run did.
#[derive(Clone, Debug, Default)]
pub struct ArchiveReport {
    /// HTML documents held.
    pub documents: usize,
    /// Stylesheets held.
    pub stylesheets: usize,
    /// Other files held (images, scripts, fonts, and payloads that only looked like pages).
    pub assets: usize,
    /// Files left alone: assets the server confirmed current, and documents whose rewritten
    /// bytes were already on disk.
    pub unchanged: usize,
    /// Files created or whose content changed on disk, plus assets whose body arrived
    /// because the server offered nothing to ask about.
    pub written: usize,
    /// Bytes actually transferred, which is more than `written` implies: pages are re-read
    /// even when they turn out to be unchanged.
    pub bytes: u64,
    /// References left pointing at the live site, because their target was not archived.
    pub remote_links: usize,
    /// Per-URL failures. A URL that fails does not stop the run.
    pub failures: Vec<String>,
    /// Set when the run stopped early because the download budget ran out.
    pub quota_paused: bool,
    /// Validators for the assets held, keyed by [`url_key`], for the next run to ask about.
    pub validators: BTreeMap<String, Validator>,
}

impl ArchiveReport {
    /// True when the archive did not change on disk — which is what a scheduled run wants to
    /// be able to say.
    pub fn is_unchanged(&self) -> bool {
        self.written == 0 && self.failures.is_empty() && !self.quota_paused
    }

    pub fn held(&self) -> usize {
        self.documents + self.stylesheets + self.assets
    }
}

// ---------------------------------------------------------------------------
// Where a URL lives on disk
// ---------------------------------------------------------------------------

/// Where an archive keeps its record of what the server said about each asset.
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(BOOKKEEPING_DIR).join(MANIFEST_FILE)
}

/// Read an archive's remembered validators.
///
/// A missing or unreadable manifest is not an error: it means every asset is fetched, which
/// is exactly what a first run does.
pub fn load_validators(root: &Path) -> BTreeMap<String, Validator> {
    let path = manifest_path(root);
    let Ok(bytes) = std::fs::read(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice(&bytes) {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "archive manifest is unreadable; every asset will be re-fetched");
            BTreeMap::new()
        }
    }
}

/// Remember an archive's validators for the next run.
pub fn save_validators(root: &Path, map: &BTreeMap<String, Validator>) -> Result<()> {
    let json = serde_json::to_vec_pretty(map)
        .map_err(|e| RiptideError::other(format!("serializing the archive manifest: {e}")))?;
    util::atomic_write(&manifest_path(root), &json)?;
    Ok(())
}

/// The part of a URL that decides which bytes are served *and* where they land: the host, the
/// path, and the query.
///
/// The scheme and the fragment are deliberately not part of it. `http://` and `https://` for
/// one host serve the same bytes in every case a mirror cares about, and a fragment never
/// changes what is served — so mapping them to one file is the result a reader wants rather
/// than a bug. A path ending in `/` is given `index.html`, exactly as a web server would.
fn key_parts(u: &Url) -> (String, String, Option<&str>) {
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let mut path = u.path().to_string();
    if path.ends_with('/') {
        path.push_str("index.html");
    }
    (host, path, u.query())
}

/// The identity of a URL inside an archive: host, path and query, with the fragment and
/// scheme removed.
///
/// Two URLs with the same key are the same file in the archive. This is what the per-asset
/// validators are keyed by, and what makes `/dir/` and `/dir/index.html` one document rather
/// than two.
pub fn url_key(u: &Url) -> String {
    let (host, path, query) = key_parts(u);
    match query {
        Some(q) => format!("{host}{path}?{q}"),
        None => format!("{host}{path}"),
    }
}

/// One path segment, reduced to something safe to create on any filesystem.
///
/// Bytes outside `[A-Za-z0-9_-]` and `.` become `~xx`, so the mapping is one-to-one: a name
/// written this way can be turned back into exactly one URL. `~` is itself escaped, which is
/// what keeps the scheme unambiguous. An empty segment (from `//`) becomes a bare `~`, and
/// because a literal `~` is always followed by two hex digits that can never be confused
/// with anything else.
///
/// Two name shapes are legal on POSIX and are not files on Windows, so they are escaped with
/// the same `~xx` scheme: a *trailing dot*, which Windows discards (`a.` would silently
/// become `a`, colliding with the real `a`), and a *device name* (`NUL`, `CON`, `COM1` …),
/// which Windows resolves anywhere in a path and with any extension. Both rules apply on
/// every platform, not just on Windows: an archive has to be the same mirror whichever
/// machine wrote it.
fn encode_segment(seg: &str) -> String {
    if seg.is_empty() {
        return "~".to_string();
    }
    // `.` and `..` are path traversal, not names.
    let dots_are_plain = seg != "." && seg != "..";
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        let c = b as char;
        let plain =
            c.is_ascii_alphanumeric() || c == '_' || c == '-' || (c == '.' && dots_are_plain);
        if plain {
            out.push(c);
        } else {
            escape(&mut out, b);
        }
    }
    if out.ends_with('.') {
        out.truncate(out.len() - 1);
        escape(&mut out, b'.');
    }
    if crate::platform::is_reserved_windows_name(&out) {
        // A device name is all ASCII, so the first byte is a whole character. Escaping it is
        // enough to turn `NUL` into an ordinary name.
        let first = out.as_bytes()[0];
        let rest = out[1..].to_string();
        out.clear();
        escape(&mut out, first);
        out.push_str(&rest);
    }
    out
}

/// Append `~xx` for one byte. The escape has to be unambiguous, and it is: a literal `~` is
/// itself always escaped, so `~` in an encoded name always introduces two hex digits.
fn escape(out: &mut String, b: u8) {
    use std::fmt::Write as _;
    let _ = write!(out, "~{b:02x}");
}

/// True for a path whose first segment is one of the reserved names.
///
/// Such a path cannot sit at the archive root without being able to collide with the host
/// mirrors or the bookkeeping directory, so it is filed under its own host instead. That is
/// what makes the whole scheme injective rather than "injective unless the site happens to
/// have a `_hosts` directory".
fn is_reserved_root(path: &str) -> bool {
    matches!(
        path.split('/').nth(1),
        Some(HOST_DIR) | Some(BOOKKEEPING_DIR)
    )
}

/// Where a URL lives inside the archive, relative to its root.
///
/// Deterministic (the URL alone decides) and injective (no two URLs of one host, and no two
/// hosts, share a path), with the single documented exception that `.../dir/` and
/// `.../dir/index.html` are the same document.
///
/// `start_host`'s files sit at the root so that a one-site archive reads like a copy of the
/// site. Anything else is filed under [`HOST_DIR`], because two hosts can perfectly well both
/// have a `/index.html`.
///
/// Every segment is a name that can actually be created on Windows as well as on POSIX, so a
/// mirror made on one machine is usable on the other.
pub fn local_path(u: &Url, start_host: &str) -> PathBuf {
    let (host, path, query) = key_parts(u);
    let under_hosts = host != start_host.to_ascii_lowercase() || is_reserved_root(&path);

    let mut out = PathBuf::new();
    if under_hosts {
        out.push(HOST_DIR);
        out.push(encode_segment(&host));
    }
    for seg in path.split('/').skip(1) {
        out.push(encode_segment(seg));
    }
    if out.as_os_str().is_empty() {
        // Only reachable for a URL with an empty path, which `Url` does not produce, but a
        // `PathBuf` with no components would make `write` fail obscurely.
        out.push("index.html");
    }
    if let Some(q) = query {
        // A query is part of the identity, so it has to survive into the name; it is
        // appended to the last segment through the same escaping, which is what keeps
        // `?a=1` and `?a=2` apart.
        let last = out
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.pop();
        out.push(encode_segment(&format!("{last}?{q}")));
    }
    out
}

/// A relative `href` from a document living in `from_dir` to the file at `to`, both
/// archive-relative.
///
/// Note that the first argument is the document's **directory**: the caller already knows
/// which document it is rewriting, and passing the file here as well would double the `..`
/// arithmetic when the two are combined.
pub fn relative_link(from_dir: &Path, to: &Path) -> String {
    let f: Vec<&str> = normal_components(from_dir);
    let t: Vec<&str> = normal_components(to);
    let common = f.iter().zip(t.iter()).take_while(|(a, b)| a == b).count();

    let mut parts: Vec<&str> = Vec::with_capacity(f.len() - common + t.len() - common);
    parts.extend(std::iter::repeat_n("..", f.len() - common));
    parts.extend_from_slice(&t[common..]);
    if parts.is_empty() {
        // `from == to`. Nothing sensible to link to; the caller should not ask.
        return to
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    parts.join("/")
}

fn normal_components(p: &Path) -> Vec<&str> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rewriting
// ---------------------------------------------------------------------------

/// How many references a rewrite moved, and how many were left remote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RewriteStats {
    pub rewritten: usize,
    pub left_remote: usize,
}

/// The patterns an offline rewrite has to find. Compile once and share: a run rewrites
/// every document it holds through the same set.
pub struct Patterns {
    quoted: Regex,
    single: Regex,
    bare: Regex,
    srcset_double: Regex,
    srcset_single: Regex,
    css_url: Regex,
    css_import: Regex,
    base: Regex,
}

const ATTR: &str =
    r#"(?i)\b(?:href|src|poster|data-src|data-original|data-lazy-src|data-bg|background)\s*=\s*"#;

impl Patterns {
    /// Compile the set.
    pub fn compile() -> Patterns {
        // Note: no backreferences — the `regex` crate is a finite-automata engine and does
        // not have them. A mismatched quote in the input is tolerated rather than rejected,
        // which is the right bias for real-world markup.
        Patterns {
            quoted: Regex::new(&format!(r#"{ATTR}"([^"]*)""#)).unwrap(),
            single: Regex::new(&format!(r#"{ATTR}'([^']*)'"#)).unwrap(),
            bare: Regex::new(&format!(r#"{ATTR}([^\s"'=<>`]+)"#)).unwrap(),
            srcset_double: Regex::new(r#"(?i)\b(?:srcset|data-srcset)\s*=\s*"([^"]*)""#).unwrap(),
            srcset_single: Regex::new(r#"(?i)\b(?:srcset|data-srcset)\s*=\s*'([^']*)'"#).unwrap(),
            css_url: Regex::new(r#"(?i)url\(\s*["']?([^'")]*)["']?\s*\)"#).unwrap(),
            css_import: Regex::new(r#"(?i)@import\s+["']([^"']*)["']"#).unwrap(),
            base: Regex::new(r#"(?i)<base\s[^>]*href\s*=\s*["']([^"']*)["']"#).unwrap(),
        }
    }
}

/// Rewrite the references in an HTML document so it works from disk.
///
/// `page` is where the document was fetched from and is what relative references resolve
/// against; `page_local` is where it now lives. The two differ, which is the whole point.
pub fn rewrite_document(
    text: &str,
    page: &Url,
    page_local: &Path,
    map: &BTreeMap<String, PathBuf>,
    p: &Patterns,
) -> (String, RewriteStats) {
    let base = document_base(text, page, p);
    let from_dir = page_local.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut stats = RewriteStats::default();

    let mut out = text.to_string();
    for re in [&p.quoted, &p.single, &p.bare] {
        out = rewrite_group(&out, re, &mut stats, |raw| {
            decide(map, &from_dir, &base, raw)
        });
    }
    for re in [&p.srcset_double, &p.srcset_single] {
        out = rewrite_group(&out, re, &mut stats, |value| {
            rewrite_srcset_value(value, map, &from_dir, &base)
        });
    }
    // `url(...)` covers inline `style="..."`, `<style>` blocks and SVG, and `@import` covers
    // stylesheets pulled in by a `<style>` block.
    out = rewrite_group(&out, &p.css_url, &mut stats, |raw| {
        decide(map, &from_dir, &base, raw)
    });
    out = rewrite_group(&out, &p.css_import, &mut stats, |raw| {
        decide(map, &from_dir, &base, raw)
    });
    (out, stats)
}

/// Rewrite the `url(...)` references in a stylesheet.
pub fn rewrite_stylesheet(
    text: &str,
    css_url: &Url,
    css_local: &Path,
    map: &BTreeMap<String, PathBuf>,
    p: &Patterns,
) -> (String, RewriteStats) {
    let from_dir = css_local.parent().unwrap_or(Path::new("")).to_path_buf();
    let base = css_url.clone();
    let mut stats = RewriteStats::default();
    let mut out = rewrite_group(text, &p.css_url, &mut stats, |raw| {
        decide(map, &from_dir, &base, raw)
    });
    out = rewrite_group(&out, &p.css_import, &mut stats, |raw| {
        decide(map, &from_dir, &base, raw)
    });
    (out, stats)
}

/// Replace the first capture group of every match with whatever `f` returns.
fn rewrite_group<F>(text: &str, re: &Regex, stats: &mut RewriteStats, mut f: F) -> String
where
    F: FnMut(&str) -> Option<(String, bool)>,
{
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for caps in re.captures_iter(text) {
        let Some(value) = caps.get(1) else { continue };
        let Some((replacement, remote)) = f(value.as_str()) else {
            continue;
        };
        if remote {
            stats.left_remote += 1;
        } else {
            stats.rewritten += 1;
        }
        out.push_str(&text[last..value.start()]);
        out.push_str(&replacement);
        last = value.end();
    }
    out.push_str(&text[last..]);
    out
}

/// `srcset="a.png 1x, b.png 2x"` — rewrite the candidates, keep the descriptors.
fn rewrite_srcset_value(
    value: &str,
    map: &BTreeMap<String, PathBuf>,
    from_dir: &Path,
    base: &Url,
) -> Option<(String, bool)> {
    let mut changed = false;
    let mut remote = false;
    let mut parts: Vec<String> = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let mut bits = item.splitn(2, char::is_whitespace);
        let url = bits.next().unwrap_or("");
        let descriptor = bits.next().unwrap_or("");
        match decide(map, from_dir, base, url) {
            Some((l, false)) => {
                changed = true;
                parts.push(join_descriptor(&l, descriptor));
            }
            Some((r, true)) => {
                changed = true;
                remote = true;
                parts.push(join_descriptor(&r, descriptor));
            }
            None => parts.push(item.to_string()),
        }
    }
    if !changed {
        return None;
    }
    Some((parts.join(", "), remote))
}

fn join_descriptor(url: &str, descriptor: &str) -> String {
    if descriptor.is_empty() {
        url.to_string()
    } else {
        format!("{url} {descriptor}")
    }
}

/// The base a document's relative references resolve against: its own URL, or a `<base
/// href>` if it declares one.
fn document_base(text: &str, page: &Url, p: &Patterns) -> Url {
    match p.base.captures(text) {
        Some(c) => page.join(&c[1]).unwrap_or_else(|_| page.clone()),
        None => page.clone(),
    }
}

/// Decide what to do with one reference.
///
/// `None` means "leave the text exactly as it was": a fragment, a `data:` URI, a `mailto:` —
/// anything that is not a fetchable http(s) resource. `Some((link, true))` is a reference
/// that has to stay pointing at the live site because its target is not in the archive.
fn decide(
    map: &BTreeMap<String, PathBuf>,
    from_dir: &Path,
    base: &Url,
    raw: &str,
) -> Option<(String, bool)> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    for scheme in [
        "data:",
        "javascript:",
        "mailto:",
        "tel:",
        "blob:",
        "about:",
        "file:",
        "ws:",
        "wss:",
        "ftp:",
    ] {
        if lower.starts_with(scheme) {
            return None;
        }
    }
    let resolved = base.join(raw).ok()?;
    if !matches!(resolved.scheme(), "http" | "https") {
        return None;
    }
    let fragment = resolved.fragment().unwrap_or("").to_string();
    match map.get(&url_key(&resolved)) {
        Some(local) => {
            let mut link = relative_link(from_dir, local);
            if !fragment.is_empty() {
                link.push('#');
                link.push_str(&fragment);
            }
            Some((link, false))
        }
        // Not held. Point at the live site rather than at a path that is not there.
        None => Some((resolved.to_string(), true)),
    }
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Role {
    Page,
    Stylesheet,
    Asset,
    Ignore,
}

fn role_of(u: &Url) -> Role {
    match grabber::extension_of(u.as_str()) {
        Some(e) if DOCUMENT_EXTENSIONS.contains(&e.as_str()) => Role::Page,
        Some(e) if STYLESHEET_EXTENSIONS.contains(&e.as_str()) => Role::Stylesheet,
        Some(e) if ASSET_EXTENSIONS.contains(&e.as_str()) => Role::Asset,
        Some(_) => Role::Ignore,
        // No extension: plenty of sites serve HTML from `/about`, and a URL whose only
        // distinguishing part is a query can be anything at all. Treated as a page, then
        // reclassified from the bytes that arrive.
        None => Role::Page,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PendingKind {
    Html,
    Css,
}

struct Pending {
    url: Url,
    local: PathBuf,
    text: String,
    kind: PendingKind,
}

struct Archiver<'a> {
    fetcher: &'a Fetcher,
    cfg: &'a ArchiveConfig,
    root: &'a Path,
    patterns: Patterns,
    known: BTreeMap<String, Validator>,
    start_host: String,
    queue: VecDeque<(Url, usize)>,
    visited: HashSet<String>,
    map: BTreeMap<String, PathBuf>,
    pending: Vec<Pending>,
    report: ArchiveReport,
    pages: usize,
    assets_fetched: usize,
}

/// Validate the page an archive starts from, and return it.
///
/// An archive of a file is a download, and the error says so rather than producing an empty
/// directory.
pub fn start_page(start: &str) -> Result<Url> {
    let url = Url::parse(start).map_err(RiptideError::Url)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(RiptideError::other(format!(
            "an archive needs an http(s) page to start from, not a {} URL",
            url.scheme()
        )));
    }
    if role_of(&url) != Role::Page {
        return Err(RiptideError::other(format!(
            "{start} is a file, not a page: an archive starts from a document, and a plain \
             file is what `riptide download` and `riptide grab --download` are for"
        )));
    }
    Ok(url)
}

/// Archive `start` and what it needs into `root`.
///
/// What the last run learned about each asset is read from, and written back to,
/// [`manifest_path`]; a first run finds no manifest and simply fetches everything.
pub async fn archive(
    fetcher: &Fetcher,
    start: &str,
    cfg: &ArchiveConfig,
    root: &Path,
) -> Result<ArchiveReport> {
    let start_url = start_page(start)?;
    let known = load_validators(root);

    let mut a = Archiver {
        fetcher,
        cfg,
        root,
        patterns: Patterns::compile(),
        known,
        start_host: start_url.host_str().unwrap_or("").to_ascii_lowercase(),
        queue: VecDeque::new(),
        visited: HashSet::new(),
        map: BTreeMap::new(),
        pending: Vec::new(),
        report: ArchiveReport::default(),
        pages: 0,
        assets_fetched: 0,
    };
    a.queue.push_back((start_url, 0));

    while let Some((url, depth)) = a.queue.pop_front() {
        let key = url_key(&url);
        if !a.visited.insert(key.clone()) {
            continue;
        }
        let local = local_path(&url, &a.start_host);
        let shown = url.to_string();
        let result = match role_of(&url) {
            Role::Page => a.fetch_page(url, depth, key, local).await,
            Role::Stylesheet => a.fetch_stylesheet(url, depth, key, local).await,
            Role::Asset => a.fetch_asset(url, key, local).await,
            Role::Ignore => Ok(()),
        };
        match result {
            Ok(()) => {}
            // The budget is a process-wide resource that refills on its own, so this is not
            // a failure: it is the run stopping where it is.
            Err(RiptideError::QuotaExhausted(msg)) => {
                a.report.quota_paused = true;
                a.report.failures.push(format!("download quota: {msg}"));
                break;
            }
            Err(RiptideError::Cancelled) => break,
            Err(e) => {
                let detail = format!("{shown}: {e}");
                tracing::debug!(%detail, "archive fetch failed");
                a.report.failures.push(detail);
            }
        }
    }

    a.write_pending()?;
    // Written even when the run stopped on the quota: the files that did land are real, and
    // their validators are what keeps the next run from fetching them again.
    save_validators(root, &a.report.validators)?;
    Ok(a.report)
}

impl Archiver<'_> {
    /// Add a discovered reference to the queue, if it is something an offline copy wants.
    fn enqueue(&mut self, raw: &str, depth: usize) {
        let Ok(u) = Url::parse(raw) else { return };
        if self.visited.contains(&url_key(&u)) {
            return;
        }
        let next = match role_of(&u) {
            Role::Page => {
                // Only page-to-page hops are limited by depth.
                if depth >= self.cfg.grab.depth {
                    return;
                }
                depth + 1
            }
            Role::Stylesheet | Role::Asset => depth,
            Role::Ignore => return,
        };
        // The grabber's own include/exclude/noise/host filters, so `--exclude` means the
        // same thing in both modes and there is only one filter to understand.
        let kept = grabber::filter_urls(
            vec![u.to_string()],
            &self.cfg.grab,
            Some(self.start_host.as_str()),
        );
        if kept.is_empty() {
            return;
        }
        self.queue.push_back((u, next));
    }

    async fn fetch_page(
        &mut self,
        url: Url,
        depth: usize,
        key: String,
        local: PathBuf,
    ) -> Result<()> {
        if self.pages >= self.cfg.grab.max_pages {
            tracing::warn!(
                limit = self.cfg.grab.max_pages,
                "archive page limit reached"
            );
            return Ok(());
        }
        self.pages += 1;

        let url_s = url.to_string();
        let data = self
            .fetcher
            .to_bytes_cond(&url_s, self.cfg.max_doc_bytes, &[])
            .await?;
        let Conditional::Fresh((bytes, meta)) = data else {
            return Ok(());
        };
        self.report.bytes = self.report.bytes.saturating_add(bytes.len() as u64);

        if !is_html_document(&url, meta.content_type.as_deref(), &bytes) {
            // A URL that looked like a page turned out to be a payload (`/asset?id=1`).
            // Keep the bytes; there is nothing in them to rewrite.
            self.store(&local, &bytes)?;
            self.map.insert(key, local);
            self.report.assets += 1;
            return Ok(());
        }

        let (text, lossy) = decode_text(&bytes, meta.content_type.as_deref());
        if lossy {
            tracing::warn!(url = %url_s, "document is not valid UTF-8; re-encoded with replacements");
        }
        self.map.insert(key, local.clone());
        // Follow the references before deciding anything else: the map has to be complete
        // before the rewriting starts, or a link to a page archived later would be left
        // pointing at the live site for no reason.
        for link in grabber::extract_urls(&text, &url) {
            self.enqueue(&link, depth);
        }
        self.pending.push(Pending {
            url,
            local,
            text,
            kind: PendingKind::Html,
        });
        Ok(())
    }

    async fn fetch_stylesheet(
        &mut self,
        url: Url,
        depth: usize,
        key: String,
        local: PathBuf,
    ) -> Result<()> {
        let url_s = url.to_string();
        // Like a document: piped through unconditionally, because the `url(...)` references
        // inside are how this stylesheet's images and fonts get discovered at all.
        let data = self
            .fetcher
            .to_bytes_cond(&url_s, self.cfg.max_doc_bytes, &[])
            .await?;
        let Conditional::Fresh((bytes, meta)) = data else {
            return Ok(());
        };
        self.report.bytes = self.report.bytes.saturating_add(bytes.len() as u64);
        let (text, lossy) = decode_text(&bytes, meta.content_type.as_deref());
        if lossy {
            tracing::warn!(url = %url_s, "stylesheet is not valid UTF-8; re-encoded with replacements");
        }
        self.map.insert(key, local.clone());
        for link in grabber::extract_urls(&text, &url) {
            self.enqueue(&link, depth);
        }
        self.pending.push(Pending {
            url,
            local,
            text,
            kind: PendingKind::Css,
        });
        Ok(())
    }

    async fn fetch_asset(&mut self, url: Url, key: String, local: PathBuf) -> Result<()> {
        if self.assets_fetched >= self.cfg.max_assets {
            tracing::warn!(limit = self.cfg.max_assets, "archive asset limit reached");
            return Ok(());
        }
        self.assets_fetched += 1;

        let cond = match self.known.get(&key) {
            Some(v) => v.headers(),
            None => Vec::new(),
        };
        let dest = self.root.join(&local);
        match self
            .fetcher
            .to_file_cond(url.as_ref(), &dest, &cond, self.cfg.max_asset_bytes)
            .await?
        {
            Conditional::NotModified => {
                self.report.unchanged += 1;
                if let Some(v) = self.known.get(&key) {
                    self.report.validators.insert(key.clone(), v.clone());
                }
            }
            Conditional::Fresh((n, meta)) => {
                self.report.bytes = self.report.bytes.saturating_add(n);
                self.report.written += 1;
                self.report
                    .validators
                    .insert(key.clone(), Validator::from_meta(&meta));
            }
        }
        self.report.assets += 1;
        self.map.insert(key, local);
        Ok(())
    }

    /// Write a payload that only looked like a page.
    fn store(&mut self, local: &Path, bytes: &[u8]) -> Result<()> {
        if write_if_changed(&self.root.join(local), bytes)? {
            self.report.written += 1;
        } else {
            self.report.unchanged += 1;
        }
        Ok(())
    }

    fn write_pending(&mut self) -> Result<()> {
        for p in std::mem::take(&mut self.pending) {
            let (text, stats) = match p.kind {
                PendingKind::Html => {
                    rewrite_document(&p.text, &p.url, &p.local, &self.map, &self.patterns)
                }
                PendingKind::Css => {
                    rewrite_stylesheet(&p.text, &p.url, &p.local, &self.map, &self.patterns)
                }
            };
            self.report.remote_links += stats.left_remote;
            match write_if_changed(&self.root.join(&p.local), text.as_bytes()) {
                Ok(true) => self.report.written += 1,
                Ok(false) => self.report.unchanged += 1,
                Err(e) => self.report.failures.push(format!("{}: {e}", p.url)),
            }
            match p.kind {
                PendingKind::Html => self.report.documents += 1,
                PendingKind::Css => self.report.stylesheets += 1,
            }
        }
        Ok(())
    }
}

/// Write only when the bytes differ, so an unchanged archive does not churn mtimes and a
/// scheduled run can honestly report that nothing changed.
fn write_if_changed(path: &Path, data: &[u8]) -> std::io::Result<bool> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == data {
            return Ok(false);
        }
    }
    util::atomic_write(path, data)?;
    Ok(true)
}

/// Whether a page-shaped URL really is markup.
///
/// A declared type wins over the URL's extension — a `/page.php` that serves a JPEG is a
/// JPEG — except for the `octet-stream` cop-outs, which say nothing at all and are extremely
/// common on static hosts.
fn is_html_document(url: &Url, content_type: Option<&str>, bytes: &[u8]) -> bool {
    if let Some(ct) = content_type {
        let mime = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match mime.as_str() {
            "text/html" | "application/xhtml+xml" | "application/xml" | "text/xml" => return true,
            "" | "application/octet-stream" | "binary/octet-stream" | "text/plain" => {}
            _ => return false,
        }
    }
    match grabber::extension_of(url.as_str()) {
        Some(e) => DOCUMENT_EXTENSIONS.contains(&e.as_str()),
        None => looks_like_markup(bytes),
    }
}

fn looks_like_markup(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let head = head.trim_start();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || head.starts_with("<?xml")
        || head.contains("<html")
        || head.contains("<head")
        || head.contains("<body")
}

/// Windows-1252 and ISO-8859-1 agree except in `0x80..=0x9F`.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{81}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{8D}', '\u{017D}', '\u{8F}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{9D}', '\u{017E}', '\u{0178}',
];

/// Decode a document's bytes using the charset the server declared.
///
/// Returns `(text, lossy)`. Only UTF-8 and the single-byte encodings are handled; anything
/// else is decoded as UTF-8 with replacements and reported, because silently mangling a page
/// and then writing it back is worse than saying so.
fn decode_text(bytes: &[u8], content_type: Option<&str>) -> (String, bool) {
    let charset = content_type.and_then(charset_of);
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "latin-1" | "us-ascii" | "ascii") => {
            (bytes.iter().map(|b| *b as char).collect(), false)
        }
        Some("windows-1252" | "cp1252" | "cp-1252") => (
            bytes
                .iter()
                .map(|b| match *b {
                    0x80..=0x9F => CP1252_HIGH[(*b - 0x80) as usize],
                    b => b as char,
                })
                .collect(),
            false,
        ),
        _ => match std::str::from_utf8(bytes) {
            Ok(s) => (s.to_string(), false),
            Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
        },
    }
}

fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|part| {
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("charset") {
            Some(v.trim().trim_matches('"').to_ascii_lowercase())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn map_of(entries: &[(&str, &str)]) -> BTreeMap<String, PathBuf> {
        entries
            .iter()
            .map(|(url, path)| (url_key(&u(url)), PathBuf::from(path)))
            .collect()
    }

    // ------------------------------------------------------------------- placement

    #[test]
    fn a_url_is_placed_by_its_own_path() {
        let p = |s: &str| local_path(&u(s), "site.test");
        assert_eq!(p("https://site.test/"), PathBuf::from("index.html"));
        assert_eq!(p("https://site.test/a/b.html"), PathBuf::from("a/b.html"));
        assert_eq!(p("https://site.test/dir/"), PathBuf::from("dir/index.html"));
        assert_eq!(
            p("https://site.test/img/a%20b.png"),
            PathBuf::from("img/a~2520b.png")
        );
    }

    #[test]
    fn two_urls_never_share_a_path() {
        // Every pair here differs only in something a naive "sanitise the file name"
        // scheme would throw away.
        let urls = [
            "https://site.test/a",
            "https://site.test/a/",
            "https://site.test/a?x=1",
            "https://site.test/a?x=2",
            "https://site.test/a?x=1&y=2",
            "https://site.test/a%3Fx=1",
            "https://site.test/a/b",
            "https://site.test/a//b",
            "https://site.test/A",
            "https://site.test/a~b",
            "https://site.test/a~3Fb",
            "https://site.test/a.b",
            "https://site.test/a_b",
            "https://site.test/a b",
            "https://site.test/a.",
            "https://site.test/index.html",
        ];
        let mut seen: BTreeMap<PathBuf, String> = BTreeMap::new();
        for s in urls {
            let path = local_path(&u(s), "site.test");
            if let Some(prev) = seen.get(&path) {
                panic!("{prev} and {s} both landed on {}", path.display());
            }
            seen.insert(path, s.to_string());
        }
    }

    #[test]
    fn the_only_deliberate_alias_is_a_directory_and_its_index() {
        assert_eq!(
            local_path(&u("https://site.test/dir/"), "site.test"),
            local_path(&u("https://site.test/dir/index.html"), "site.test")
        );
        assert_eq!(
            url_key(&u("https://site.test/dir/")),
            url_key(&u("https://site.test/dir/index.html"))
        );
    }

    #[test]
    fn nothing_can_climb_out_of_the_archive_root() {
        for s in [
            "https://site.test/../etc/passwd",
            "https://site.test/%2e%2e/%2e%2e/etc/passwd",
            "https://site.test/a/../../x",
            "https://site.test/.",
        ] {
            let path = local_path(&u(s), "site.test");
            assert!(
                !path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::RootDir)),
                "{s} produced {}",
                path.display()
            );
        }
    }

    #[test]
    fn other_hosts_get_their_own_tree() {
        let same = local_path(&u("https://site.test/index.html"), "site.test");
        let other = local_path(&u("https://cdn.test/index.html"), "site.test");
        assert_eq!(same, PathBuf::from("index.html"));
        assert_eq!(other, PathBuf::from("_hosts/cdn.test/index.html"));
        assert_ne!(same, other);
    }

    #[test]
    fn a_site_that_uses_the_reserved_name_cannot_collide_with_a_host_mirror() {
        // The start host's own `_hosts/` directory is filed under its host, so it cannot
        // land on top of `cdn.test/`'s mirror.
        let mine = local_path(
            &u("https://site.test/_hosts/cdn.test/index.html"),
            "site.test",
        );
        let theirs = local_path(&u("https://cdn.test/index.html"), "site.test");
        assert_eq!(
            mine,
            PathBuf::from("_hosts/site.test/_hosts/cdn.test/index.html")
        );
        assert_ne!(mine, theirs);

        // The bookkeeping directory gets the same treatment, so a site that serves
        // `/_riptide/validators.json` cannot overwrite the archive's own record.
        let theirs = local_path(
            &u("https://site.test/_riptide/validators.json"),
            "site.test",
        );
        assert_eq!(
            theirs,
            PathBuf::from("_hosts/site.test/_riptide/validators.json")
        );
        assert_ne!(theirs, manifest_path(Path::new("")));
    }

    #[test]
    fn the_manifest_round_trips_and_a_missing_one_is_simply_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(load_validators(root).is_empty());

        let mut map = BTreeMap::new();
        map.insert(
            url_key(&u("https://site.test/img/a.png")),
            Validator {
                etag: Some("\"abc\"".into()),
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            },
        );
        save_validators(root, &map).unwrap();
        assert!(manifest_path(root).exists());
        let back = load_validators(root);
        assert_eq!(back, map);

        // A corrupt manifest is not a hard failure: the archive just stops being incremental.
        std::fs::write(manifest_path(root), b"not json").unwrap();
        assert!(load_validators(root).is_empty());
    }

    #[test]
    fn the_scheme_and_fragment_do_not_split_a_resource() {
        let a = url_key(&u("http://site.test/x"));
        assert_eq!(a, url_key(&u("https://site.test/x")));
        assert_eq!(a, url_key(&u("https://site.test/x#part")));
        assert_eq!(
            url_key(&u("https://site.test/dir/")),
            url_key(&u("https://site.test/dir/index.html"))
        );
        assert_ne!(
            url_key(&u("https://site.test/x")),
            url_key(&u("https://other.test/x"))
        );
    }

    #[test]
    fn relative_links_walk_up_and_back_down() {
        let r = |a: &str, b: &str| relative_link(Path::new(a), Path::new(b));
        assert_eq!(r("a/b", "a/b/style.css"), "style.css");
        assert_eq!(r("a/b", "a/x.png"), "../x.png");
        assert_eq!(r("a/b", "index.html"), "../../index.html");
        assert_eq!(r("", "a/b.png"), "a/b.png");
        assert_eq!(
            r("a", "_hosts/cdn.test/lib.js"),
            "../_hosts/cdn.test/lib.js"
        );
    }

    // ------------------------------------------------------------------- rewriting

    fn rewritten(html: &str, map: &BTreeMap<String, PathBuf>, local: &str) -> String {
        let p = Patterns::compile();
        let (out, _) = rewrite_document(
            html,
            &u("https://site.test/dir/page.html"),
            Path::new(local),
            map,
            &p,
        );
        out
    }

    #[test]
    fn links_to_held_files_become_relative_and_the_rest_stay_absolute() {
        let map = map_of(&[
            ("https://site.test/dir/style.css", "dir/style.css"),
            ("https://site.test/img/logo.png", "img/logo.png"),
        ]);
        let html = r##"
            <link rel="stylesheet" href="style.css">
            <img src="../img/logo.png">
            <a href="/missing/page.html">gone</a>
            <a href="#top">anchor</a>
            <a href="mailto:x@y.z">mail</a>
            <script src="data:text/javascript,0"></script>
            <a href="https://other.test/x">offsite</a>
        "##;
        let out = rewritten(html, &map, "dir/page.html");
        assert!(out.contains(r#"href="style.css""#), "{out}");
        assert!(out.contains(r#"src="../img/logo.png""#), "{out}");
        assert!(
            out.contains(r#"href="https://site.test/missing/page.html""#),
            "a link we did not archive has to stay reachable: {out}"
        );
        assert!(out.contains(r##"href="#top""##), "{out}");
        assert!(out.contains(r#"href="mailto:x@y.z""#), "{out}");
        assert!(out.contains("data:text/javascript,0"), "{out}");
        assert!(out.contains(r#"href="https://other.test/x""#), "{out}");
    }

    #[test]
    fn fragments_survive_a_rewrite() {
        let map = map_of(&[("https://site.test/dir/next.html", "dir/next.html")]);
        let out = rewritten(
            r#"<a href="next.html#section-3">x</a>"#,
            &map,
            "dir/page.html",
        );
        assert!(out.contains(r#"href="next.html#section-3""#), "{out}");
    }

    #[test]
    fn unquoted_and_single_quoted_attributes_are_rewritten() {
        let map = map_of(&[
            ("https://site.test/dir/a.css", "dir/a.css"),
            ("https://site.test/dir/b.js", "dir/b.js"),
        ]);
        let out = rewritten(
            "<link href=style-absent.css><script src='a.css'></script><i background=b.js>x</i>",
            &map,
            "dir/page.html",
        );
        assert!(out.contains("src='a.css'"), "{out}");
        assert!(out.contains("background=b.js"), "{out}");
    }

    #[test]
    fn srcset_descriptors_are_kept() {
        let map = map_of(&[
            ("https://site.test/dir/s1.png", "dir/s1.png"),
            ("https://site.test/dir/s2.png", "dir/s2.png"),
        ]);
        let out = rewritten(
            r#"<img srcset="s1.png 1x, s2.png 2x">"#,
            &map,
            "dir/page.html",
        );
        assert!(out.contains(r#"srcset="s1.png 1x, s2.png 2x""#), "{out}");
    }

    #[test]
    fn inline_style_urls_and_css_imports_are_rewritten() {
        let map = map_of(&[
            ("https://site.test/dir/bg.png", "dir/bg.png"),
            ("https://site.test/dir/extra.css", "dir/extra.css"),
        ]);
        let out = rewritten(
            r#"<div style="background:url('bg.png')"></div><style>@import "extra.css";</style>"#,
            &map,
            "dir/page.html",
        );
        assert!(out.contains("url('bg.png')"), "{out}");
        assert!(out.contains(r#"@import "extra.css""#), "{out}");
    }

    #[test]
    fn a_stylesheet_is_rewritten_from_its_own_directory() {
        let p = Patterns::compile();
        // Nothing that the stylesheet points at is held, so every reference has to stay
        // pointing at the live site rather than at a path that is not there.
        let empty = map_of(&[("https://site.test/other/x", "other/x")]);
        let (out, stats) = rewrite_stylesheet(
            "@font-face{src:url(../fonts/font.woff2)} body{background:url(/img/bg.png)}",
            &u("https://site.test/css/main.css"),
            Path::new("css/main.css"),
            &empty,
            &p,
        );
        assert_eq!(stats.rewritten, 0, "{out}");
        assert_eq!(stats.left_remote, 2);
        assert!(
            out.contains("url(https://site.test/fonts/font.woff2)"),
            "{out}"
        );
        assert!(out.contains("url(https://site.test/img/bg.png)"), "{out}");

        let held = map_of(&[
            ("https://site.test/fonts/font.woff2", "fonts/font.woff2"),
            ("https://site.test/img/bg.png", "img/bg.png"),
        ]);
        let (out, stats) = rewrite_stylesheet(
            "@font-face{src:url(../fonts/font.woff2)} body{background:url(/img/bg.png)}",
            &u("https://site.test/css/main.css"),
            Path::new("css/main.css"),
            &held,
            &p,
        );
        assert_eq!(stats.rewritten, 2, "{out}");
        assert_eq!(stats.left_remote, 0);
        assert!(out.contains("url(../fonts/font.woff2)"), "{out}");
        assert!(out.contains("url(../img/bg.png)"), "{out}");
    }

    #[test]
    fn a_base_element_redirects_relative_resolution() {
        let map = map_of(&[("https://site.test/real/style.css", "real/style.css")]);
        let p = Patterns::compile();
        let html = r#"<base href="/real/"><link href="style.css">"#;
        let (out, stats) = rewrite_document(
            html,
            &u("https://site.test/deep/page.html"),
            Path::new("deep/page.html"),
            &map,
            &p,
        );
        assert_eq!(stats.rewritten, 1, "{out}");
        assert!(out.contains(r#"href="../real/style.css""#), "{out}");
    }

    #[test]
    fn rewriting_is_idempotent_on_its_own_output() {
        let map = map_of(&[("https://site.test/dir/a.css", "dir/a.css")]);
        let first = rewritten(r#"<link href="a.css">"#, &map, "dir/page.html");
        let second = rewritten(&first, &map, "dir/page.html");
        assert_eq!(first, second);
    }

    // ------------------------------------------------------------------- role and type

    #[test]
    fn roles_separate_what_a_page_needs_from_what_a_person_downloads() {
        assert_eq!(role_of(&u("https://x/a.html")), Role::Page);
        assert_eq!(role_of(&u("https://x/about")), Role::Page);
        assert_eq!(role_of(&u("https://x/dir/")), Role::Page);
        assert_eq!(role_of(&u("https://x/a.css")), Role::Stylesheet);
        assert_eq!(role_of(&u("https://x/a.js")), Role::Asset);
        assert_eq!(role_of(&u("https://x/a.woff2")), Role::Asset);
        // Things to download, not things to render.
        assert_eq!(role_of(&u("https://x/movie.mp4")), Role::Ignore);
        assert_eq!(role_of(&u("https://x/archive.zip")), Role::Ignore);
        assert_eq!(role_of(&u("https://x/paper.pdf")), Role::Ignore);
    }

    #[test]
    fn a_declared_type_beats_the_extension_except_when_it_says_nothing() {
        let b = b"<html><body>hi</body></html>";
        assert!(is_html_document(
            &u("https://x/page.php"),
            Some("text/html"),
            b
        ));
        assert!(!is_html_document(
            &u("https://x/page.php"),
            Some("image/jpeg"),
            b
        ));
        // `octet-stream` is the classic "we did not bother" answer from a static host.
        assert!(is_html_document(
            &u("https://x/page.html"),
            Some("application/octet-stream"),
            b
        ));
        assert!(is_html_document(&u("https://x/thing"), None, b));
        assert!(!is_html_document(
            &u("https://x/thing"),
            None,
            &[0x89, b'P', b'N', b'G', 0x0d]
        ));
    }

    #[test]
    fn single_byte_encodings_are_decoded_rather_than_mangled() {
        assert_eq!(
            decode_text(&[0xE9, 0x74, 0xE9], Some("text/html; charset=iso-8859-1")).0,
            "été"
        );
        // 0x80 is the euro sign in windows-1252 and a control character in latin-1.
        assert_eq!(
            decode_text(&[0x80], Some("text/html; charset=windows-1252")).0,
            "€"
        );
        assert_eq!(
            decode_text("été".as_bytes(), Some("text/html; charset=utf-8")).0,
            "été"
        );
        let (text, lossy) = decode_text(&[0xff, 0xfe], Some("text/html; charset=utf-8"));
        assert!(lossy);
        assert!(!text.is_empty());
    }

    #[test]
    fn a_charset_is_found_however_it_is_spelled() {
        assert_eq!(
            charset_of("text/html; charset=UTF-8").as_deref(),
            Some("utf-8")
        );
        assert_eq!(
            charset_of("text/html;charset=\"Windows-1252\"").as_deref(),
            Some("windows-1252")
        );
        assert_eq!(charset_of("text/html"), None);
    }

    #[test]
    fn segment_escaping_round_trips_for_the_cases_that_matter() {
        assert_eq!(encode_segment("a b"), "a~20b");
        assert_eq!(encode_segment("~"), "~7e");
        assert_eq!(encode_segment(""), "~");
        // A literal `~7e` must not be readable as the escape for `~`.
        assert_ne!(encode_segment("~"), encode_segment("~7e"));
        assert_ne!(encode_segment(".."), "..");
    }

    #[test]
    fn a_segment_that_windows_would_not_store_is_escaped_the_same_everywhere() {
        // A trailing dot is dropped by Windows, so `..` must not be left as a name that
        // becomes “” there. The escape happens on every platform, so a Linux mirror is not
        // full of names Windows cannot represent.
        assert_eq!(encode_segment("..."), "..~2e");
        assert_eq!(encode_segment("a."), "a~2e");
        assert_eq!(encode_segment("trailing. "), "trailing.~20");
        // `NUL`, `CON`, `COM1` … are devices on Windows, with or without an extension.
        assert_eq!(encode_segment("NUL"), "~4eUL");
        assert_eq!(encode_segment("nul.txt"), "~6eul.txt");
        assert_eq!(encode_segment("COM1"), "~43OM1");
        // …but only the documented ones: `COM10` is an ordinary file.
        assert_eq!(encode_segment("COM10"), "COM10");
        assert_eq!(encode_segment("AUX.log"), "~41UX.log");
        // A leading dot is not a trailing dot, and `.hidden` is a fine name on both.
        assert_eq!(encode_segment(".hidden"), ".hidden");
        // The escapes stay injective: no BYTE ever produces `~4e` except the device rule,
        // because `N` is plain and a literal `~` is written `~7e`.
        assert_ne!(encode_segment("NUL"), encode_segment("~4eUL"));
        assert_ne!(encode_segment("a."), encode_segment("a.~2e"));
    }

    #[test]
    fn an_archive_refuses_to_start_from_something_that_is_not_a_page() {
        assert!(start_page("https://x/index.html").is_ok());
        assert!(start_page("https://x/").is_ok());
        assert!(start_page("https://x/about").is_ok());
        // A file is a download, and saying so is more useful than an empty directory.
        assert!(start_page("https://x/movie.mp4").is_err());
        assert!(start_page("https://x/archive.zip").is_err());
        assert!(start_page("ftp://x/index.html").is_err());
        assert!(start_page("not a url").is_err());
    }
}
