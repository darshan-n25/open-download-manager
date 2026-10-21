//! HLS (`application/vnd.apple.mpegurl`) parsing and resolution.
//!
//! Mirrors IDM's `CM3UParser` family: `MasterPlaylist`, `MediaPlaylist`, `MPLVariant`,
//! `MediaSegment` and a buffered `decodingState`.

use url::Url;

use crate::engine::Engine;
use crate::error::{OdmError, Result};
use crate::job::{MediaKey, MediaSegmentRef, MediaSpec};

/// One `#EXT-X-STREAM-INF` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub url: Url,
    pub bandwidth: Option<u64>,
    pub average_bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub audio_group: Option<String>,
    pub name: Option<String>,
}

impl Variant {
    pub fn label(&self) -> String {
        match (self.width, self.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => match self.bandwidth {
                Some(b) => format!("{:.0} kbps", b as f64 / 1000.0),
                None => "unknown".to_string(),
            },
        }
    }
}

/// One `#EXT-X-MEDIA:TYPE=AUDIO` rendition.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendition {
    pub group_id: String,
    pub name: Option<String>,
    pub language: Option<String>,
    pub uri: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub audio: Vec<Rendition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlaylist {
    pub segments: Vec<MediaSegmentRef>,
    pub init_url: Option<String>,
    pub key: Option<MediaKey>,
    pub target_duration: Option<u32>,
    pub end_list: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(Master),
    Media(MediaPlaylist),
}

/// Split an attribute list on commas that are not inside double quotes.
fn split_attrs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' if !in_quotes => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Parse `KEY=VALUE` pairs, stripping surrounding quotes.
fn attrs(s: &str) -> Vec<(String, String)> {
    split_attrs(s)
        .into_iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let v = v.trim().trim_matches('"').to_string();
            Some((k.trim().to_ascii_uppercase(), v))
        })
        .collect()
}

fn attr<'a>(list: &'a [(String, String)], key: &str) -> Option<&'a str> {
    list.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn parse_resolution(v: &str) -> (Option<u32>, Option<u32>) {
    match v.split_once('x') {
        Some((w, h)) => (w.trim().parse().ok(), h.trim().parse().ok()),
        None => (None, None),
    }
}

fn parse_iv(v: &str) -> Option<Vec<u8>> {
    let hex_part = v.trim().trim_start_matches("0x").trim_start_matches("0X");
    let clean: String = hex_part.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if !clean.len().is_multiple_of(2) || clean.is_empty() {
        return None;
    }
    hex::decode(clean).ok().filter(|b| b.len() == 16)
}

/// Parse an M3U8 document relative to `base`.
pub fn parse(text: &str, base: &Url) -> Result<Playlist> {
    let mut variants: Vec<Variant> = Vec::new();
    let mut renditions: Vec<Rendition> = Vec::new();
    let mut segments: Vec<MediaSegmentRef> = Vec::new();
    let mut init_url: Option<String> = None;
    let mut key: Option<MediaKey> = None;
    let mut target_duration = None;
    let mut end_list = false;

    let mut pending_variant: Option<Vec<(String, String)>> = None;
    let mut pending_duration: Option<f64> = None;
    let mut pending_range: Option<(u64, u64)> = None;
    let mut seq: u64 = 0;
    let mut byte_cursor: u64 = 0;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_variant = Some(attrs(rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = attrs(rest);
            if attr(&a, "TYPE").map(|t| t.eq_ignore_ascii_case("AUDIO")) == Some(true) {
                renditions.push(Rendition {
                    group_id: attr(&a, "GROUP-ID").unwrap_or("").to_string(),
                    name: attr(&a, "NAME").map(|s| s.to_string()),
                    language: attr(&a, "LANGUAGE").map(|s| s.to_string()),
                    uri: attr(&a, "URI").map(|s| s.to_string()),
                    is_default: attr(&a, "DEFAULT").map(|v| v.eq_ignore_ascii_case("YES"))
                        == Some(true),
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest.split(',').next().unwrap_or("").trim();
            pending_duration = dur.parse::<f64>().ok();
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let rest = rest.trim();
            let (len, off) = match rest.split_once('@') {
                Some((l, o)) => (l.trim(), Some(o.trim())),
                None => (rest, None),
            };
            let len: u64 = len
                .parse()
                .map_err(|_| OdmError::Manifest("bad EXT-X-BYTERANGE length".into()))?;
            let start = match off {
                Some(o) => o
                    .parse()
                    .map_err(|_| OdmError::Manifest("bad EXT-X-BYTERANGE offset".into()))?,
                None => byte_cursor,
            };
            byte_cursor = start + len;
            pending_range = Some((start, start + len - 1));
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attrs(rest);
            if let Some(u) = attr(&a, "URI") {
                init_url = Some(resolve_url(base, u));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attrs(rest);
            let method = attr(&a, "METHOD").unwrap_or("NONE").to_string();
            if method.eq_ignore_ascii_case("NONE") {
                key = None;
            } else {
                key = Some(MediaKey {
                    method,
                    uri: attr(&a, "URI")
                        .map(|u| resolve_url(base, u))
                        .unwrap_or_default(),
                    iv: attr(&a, "IV").and_then(parse_iv),
                    key: None,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = rest.trim().parse().ok();
            continue;
        }
        if line.starts_with("#EXT-X-ENDLIST") {
            end_list = true;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }

        // A URI line: either a variant or a segment.
        let url = resolve_url(base, line);
        if let Some(a) = pending_variant.take() {
            let (w, h) = attr(&a, "RESOLUTION")
                .map(parse_resolution)
                .unwrap_or((None, None));
            variants.push(Variant {
                url: Url::parse(&url).map_err(|e| OdmError::Manifest(e.to_string()))?,
                bandwidth: attr(&a, "BANDWIDTH").and_then(|v| v.parse().ok()),
                average_bandwidth: attr(&a, "AVERAGE-BANDWIDTH").and_then(|v| v.parse().ok()),
                width: w,
                height: h,
                codecs: attr(&a, "CODECS").map(|s| s.to_string()),
                audio_group: attr(&a, "AUDIO").map(|s| s.to_string()),
                name: attr(&a, "NAME").map(|s| s.to_string()),
            });
        } else {
            segments.push(MediaSegmentRef {
                url,
                byte_range: pending_range.take(),
                seq,
                duration: pending_duration.take(),
            });
            seq += 1;
        }
    }

    if !variants.is_empty() {
        return Ok(Playlist::Master(Master {
            variants,
            audio: renditions,
        }));
    }
    if segments.is_empty() && init_url.is_none() {
        return Err(OdmError::Manifest(
            "M3U8 contains neither variants nor segments".into(),
        ));
    }
    Ok(Playlist::Media(MediaPlaylist {
        segments,
        init_url,
        key,
        target_duration,
        end_list,
    }))
}

fn resolve_url(base: &Url, rel: &str) -> String {
    base.join(rel)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| rel.to_string())
}

/// Pick a rendition. `preference` is `best` (default), `worst`, or a height like `720`.
pub fn select_variant<'a>(
    variants: &'a [Variant],
    preference: Option<&str>,
) -> Result<&'a Variant> {
    if variants.is_empty() {
        return Err(OdmError::Manifest("no variants in master playlist".into()));
    }
    let pref = preference.unwrap_or("best").trim().to_ascii_lowercase();

    let score = |v: &Variant| -> u64 {
        let px = v.width.unwrap_or(0) as u64 * v.height.unwrap_or(0) as u64;
        px * 1_000_000 + v.bandwidth.unwrap_or(0)
    };

    if pref == "worst" {
        return Ok(variants.iter().min_by_key(|v| score(v)).unwrap());
    }
    if let Ok(want_h) = pref.parse::<u32>() {
        // Closest available whose height does not exceed the request, else the smallest.
        let mut candidates: Vec<&Variant> =
            variants.iter().filter(|v| v.height.is_some()).collect();
        if !candidates.is_empty() {
            candidates.sort_by_key(|v| v.height.unwrap());
            let exact = candidates
                .iter()
                .filter(|v| v.height.unwrap() <= want_h)
                .max_by_key(|v| v.height.unwrap());
            return Ok(*exact.unwrap_or(&candidates[0]));
        }
    }
    Ok(variants.iter().max_by_key(|v| score(v)).unwrap())
}

/// Guess the container from segment URLs.
pub fn detect_container(segments: &[MediaSegmentRef]) -> String {
    let mut ts = 0usize;
    let mut mp4 = 0usize;
    for s in segments {
        let path = s
            .url
            .split('?')
            .next()
            .unwrap_or(&s.url)
            .to_ascii_lowercase();
        if path.ends_with(".ts") || path.ends_with(".tsv") {
            ts += 1;
        } else if path.ends_with(".m4s")
            || path.ends_with(".mp4")
            || path.ends_with(".m4a")
            || path.ends_with(".cmfv")
        {
            mp4 += 1;
        }
    }
    if ts > mp4 {
        "ts".to_string()
    } else if mp4 > 0 {
        "mp4".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Turn a resolved media playlist into the flat plan the muxer consumes.
fn build_spec(manifest_url: &str, variant: Option<&Variant>, playlist: MediaPlaylist) -> MediaSpec {
    let container = detect_container(&playlist.segments);
    MediaSpec {
        manifest_url: manifest_url.to_string(),
        variant_url: variant.map(|v| v.url.to_string()),
        variant_label: variant.map(|v| v.label()),
        init_url: playlist.init_url,
        segments: playlist.segments,
        key: playlist.key,
        audio: None,
        container,
        total_bytes: None,
    }
}

/// Follow masters until a media playlist appears (at most a few levels).
async fn load_media(
    engine: &mut Engine,
    url: &str,
    preference: Option<&str>,
    depth: usize,
) -> Result<(Option<Variant>, MediaPlaylist)> {
    if depth > 3 {
        return Err(OdmError::Manifest(
            "playlist nesting is deeper than expected".into(),
        ));
    }
    let base = Url::parse(url)?;
    let bytes = engine.fetch_bytes(url, 16 << 20).await?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    match parse(&text, &base)? {
        Playlist::Media(m) => Ok((None, m)),
        Playlist::Master(m) => {
            let chosen = select_variant(&m.variants, preference)?.clone();
            let (_, media) = Box::pin(load_media(
                engine,
                chosen.url.as_str(),
                preference,
                depth + 1,
            ))
            .await?;
            Ok((Some(chosen), media))
        }
    }
}

/// Resolve an HLS URL into a [`MediaSpec`], including a separate audio track when the
/// manifest splits the two.
pub async fn resolve(engine: &mut Engine) -> Result<MediaSpec> {
    let manifest_url = engine
        .job()
        .resolved_url
        .clone()
        .unwrap_or_else(|| engine.job().url.clone());
    let preference = engine.job().quality.clone();
    let pref = preference.as_deref();

    // First pass: find out whether this is a master with renditions.
    let base = Url::parse(&manifest_url)?;
    let bytes = engine.fetch_bytes(&manifest_url, 16 << 20).await?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let parsed = parse(&text, &base)?;

    let (variant, media, audio_group) = match parsed {
        Playlist::Media(m) => (None, m, None),
        Playlist::Master(m) => {
            let chosen = select_variant(&m.variants, pref)?.clone();
            let group = chosen.audio_group.clone();

            // Audio rendition belonging to the chosen variant's group.
            let audio_url = group.as_ref().and_then(|g| {
                let mut candidates: Vec<&Rendition> = m
                    .audio
                    .iter()
                    .filter(|r| &r.group_id == g && r.uri.is_some())
                    .collect();
                candidates.sort_by_key(|r| !r.is_default);
                candidates.first().and_then(|r| r.uri.as_ref()).map(|u| {
                    base.join(u)
                        .map(|x| x.to_string())
                        .unwrap_or_else(|_| u.clone())
                })
            });

            let (_, media) = Box::pin(load_media(engine, chosen.url.as_str(), pref, 0)).await?;
            (Some(chosen), media, audio_url)
        }
    };

    let audio_url = audio_group;
    let mut spec = build_spec(&manifest_url, variant.as_ref(), media);

    if let Some(aurl) = audio_url {
        match Box::pin(load_media(engine, &aurl, pref, 0)).await {
            Ok((avar, amedia)) => {
                let mut aspec = build_spec(&aurl, avar.as_ref(), amedia);
                aspec.container = if aspec.container == "unknown" {
                    "mp4".to_string()
                } else {
                    aspec.container
                };
                spec.audio = Some(Box::new(aspec));
            }
            Err(e) => {
                tracing::warn!(error = %e, "audio rendition failed; continuing video-only");
            }
        }
    }

    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.test/hls/master.m3u8").unwrap()
    }

    #[test]
    fn parses_a_master_playlist() {
        let text = "\
#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio/en.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"
360/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aud\"
1080/index.m3u8
";
        let p = parse(text, &base()).unwrap();
        let Playlist::Master(m) = p else {
            panic!("expected master")
        };
        assert_eq!(m.variants.len(), 2);
        assert_eq!(m.audio.len(), 1);
        assert!(m.audio[0].is_default);
        assert_eq!(m.variants[1].height, Some(1080));
        assert_eq!(m.variants[1].audio_group.as_deref(), Some("aud"));
        assert_eq!(
            m.variants[0].url.as_str(),
            "https://cdn.test/hls/360/index.m3u8"
        );
        assert_eq!(m.variants[0].label(), "640x360");
    }

    #[test]
    fn parses_a_media_playlist_with_map_key_and_byteranges() {
        let text = "\
#EXTM3U
#EXT-X-TARGETDURATION:6
#EXT-X-MAP:URI=\"init.mp4\"
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x0123456789abcdef0123456789abcdef
#EXTINF:6.0,
#EXT-X-BYTERANGE:1000@0
seg.mp4
#EXTINF:6.0,
#EXT-X-BYTERANGE:2000
seg.mp4
#EXT-X-ENDLIST
";
        let p = parse(text, &base()).unwrap();
        let Playlist::Media(m) = p else {
            panic!("expected media")
        };
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.init_url.as_deref(), Some("https://cdn.test/hls/init.mp4"));
        assert_eq!(m.segments[0].byte_range, Some((0, 999)));
        // Second byterange continues from the end of the first: 1000..2999.
        assert_eq!(m.segments[1].byte_range, Some((1000, 2999)));
        assert!(m.end_list);
        let key = m.key.unwrap();
        assert_eq!(key.method, "AES-128");
        assert_eq!(
            key.iv.as_deref().map(hex::encode),
            Some("0123456789abcdef0123456789abcdef".to_string())
        );
        assert_eq!(m.segments[0].duration, Some(6.0));
    }

    #[test]
    fn key_none_clears_encryption() {
        let text = "\
#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI=\"k\"
#EXTINF:1,
a.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:1,
b.ts
";
        let Playlist::Media(m) = parse(text, &base()).unwrap() else {
            panic!()
        };
        assert!(m.key.is_none());
        assert_eq!(m.segments.len(), 2);
    }

    #[test]
    fn selection_best_worst_and_height() {
        let text = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
a.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080
b.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720
c.m3u8
";
        let Playlist::Master(m) = parse(text, &base()).unwrap() else {
            panic!()
        };
        assert_eq!(
            select_variant(&m.variants, None).unwrap().height,
            Some(1080)
        );
        assert_eq!(
            select_variant(&m.variants, Some("best")).unwrap().height,
            Some(1080)
        );
        assert_eq!(
            select_variant(&m.variants, Some("worst")).unwrap().height,
            Some(360)
        );
        assert_eq!(
            select_variant(&m.variants, Some("720")).unwrap().height,
            Some(720)
        );
        assert_eq!(
            select_variant(&m.variants, Some("800")).unwrap().height,
            Some(720)
        );
        assert_eq!(
            select_variant(&m.variants, Some("100")).unwrap().height,
            Some(360)
        );
    }

    #[test]
    fn container_detection() {
        let mk = |u: &str| MediaSegmentRef {
            url: u.into(),
            byte_range: None,
            seq: 0,
            duration: None,
        };
        assert_eq!(detect_container(&[mk("a.ts"), mk("b.ts")]), "ts");
        assert_eq!(detect_container(&[mk("a.m4s"), mk("b.m4s")]), "mp4");
        assert_eq!(detect_container(&[mk("a?x=1")]), "unknown");
    }

    #[test]
    fn rejects_a_document_with_no_streams() {
        assert!(parse("#EXTM3U\n#EXT-X-ENDLIST\n", &base()).is_err());
    }

    #[test]
    fn quoted_commas_do_not_split_attributes() {
        let a = attrs("CODECS=\"avc1.4d401e,mp4a.40.2\",BANDWIDTH=1");
        assert_eq!(attr(&a, "CODECS"), Some("avc1.4d401e,mp4a.40.2"));
        assert_eq!(attr(&a, "BANDWIDTH"), Some("1"));
    }
}
