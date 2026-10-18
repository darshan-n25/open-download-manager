//! DASH (`application/dash+xml`) parsing.
//!
//! Covers what IDM's `CMp4DashReader` covers in practice: `SegmentTemplate` with either
//! a fixed `@duration` or an explicit `SegmentTimeline`, `$RepresentationID$` /
//! `$Number$` / `$Time$` / `$Bandwidth$` substitution (including `%0Nd` padding),
//! `SegmentList`, `Initialization`, nested `BaseURL` resolution, and separate video and
//! audio `AdaptationSet`s.

use std::collections::HashMap;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use url::Url;

use crate::engine::Engine;
use crate::error::{Result, RiptideError};
use crate::job::{MediaSegmentRef, MediaSpec};

/// One `<Representation>`, flattened together with everything inherited from its
/// ancestors (BaseURL chain, AdaptationSet mime type, Period duration).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rep {
    pub id: String,
    pub bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub mime: Option<String>,
    pub base: String,
    pub init_template: Option<String>,
    pub media_template: Option<String>,
    pub timescale: u64,
    pub duration: Option<u64>,
    pub start_number: u64,
    /// `(t, d, r)` entries in document order.
    pub timeline: Vec<(Option<u64>, u64, i64)>,
    pub segment_list_init: Option<String>,
    pub segment_list_urls: Vec<String>,
    pub period_duration: Option<f64>,
}

impl Rep {
    pub fn label(&self) -> String {
        match (self.width, self.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => match self.bandwidth {
                Some(b) => format!("{:.0} kbps", b as f64 / 1000.0),
                None => self.id.clone(),
            },
        }
    }

    pub fn is_audio(&self) -> bool {
        self.mime
            .as_deref()
            .map(|m| m.starts_with("audio/"))
            .unwrap_or(false)
            || self
                .codecs
                .as_deref()
                .map(|c| c.starts_with("mp4a") || c.starts_with("opus") || c.starts_with("ac-3"))
                == Some(true)
    }

    pub fn is_video(&self) -> bool {
        self.mime
            .as_deref()
            .map(|m| m.starts_with("video/"))
            .unwrap_or(false)
    }

    fn container(&self) -> String {
        match self.mime.as_deref() {
            Some(m) if m.starts_with("audio/") => "mp4".to_string(),
            Some("video/mp4") | Some("video/iso.segment") => "mp4".to_string(),
            Some(m) if m.contains("mp2t") => "ts".to_string(),
            _ => "mp4".to_string(),
        }
    }

    /// Expand this representation into concrete segment URLs.
    pub fn segments(&self) -> Result<Vec<MediaSegmentRef>> {
        if let Some(tpl) = &self.media_template {
            let timescale = if self.timescale == 0 {
                1
            } else {
                self.timescale
            };
            let mut out = Vec::new();
            let mut seq = self.start_number.max(1);

            if !self.timeline.is_empty() {
                let mut time: u64 = 0;
                for (t, d, r) in &self.timeline {
                    if let Some(t) = t {
                        time = *t;
                    }
                    // r == -1 means "repeat until the end of the period".
                    let repeats = if *r < 0 {
                        match (self.period_duration, *d) {
                            (Some(pd), d) if d > 0 => {
                                let total = (pd * timescale as f64) as u64;
                                (total.saturating_sub(time) / d).saturating_sub(1) as i64
                            }
                            _ => 0,
                        }
                    } else {
                        *r
                    };
                    for _ in 0..(repeats + 1) {
                        if let Some(pd) = self.period_duration {
                            if (time as f64 / timescale as f64) > pd + 0.5 {
                                break;
                            }
                        }
                        let url = subst(tpl, &self.id, seq, time, self.bandwidth.unwrap_or(0));
                        out.push(MediaSegmentRef {
                            url: join(&self.base, &url),
                            byte_range: None,
                            seq,
                            duration: Some(*d as f64 / timescale as f64),
                        });
                        seq += 1;
                        time += *d;
                    }
                }
                return Ok(out);
            }

            let (Some(dur), Some(pd)) = (self.duration, self.period_duration) else {
                return Err(RiptideError::Manifest(format!(
                    "representation {:?} has a SegmentTemplate with neither a SegmentTimeline \
                     nor a usable @duration/@period duration",
                    self.id
                )));
            };
            if dur == 0 {
                return Err(RiptideError::Manifest(
                    "SegmentTemplate @duration is zero".into(),
                ));
            }
            let total_ticks = (pd * timescale as f64).ceil() as u64;
            let count = total_ticks.div_ceil(dur);
            for i in 0..count {
                let time = i * dur;
                let url = subst(tpl, &self.id, seq, time, self.bandwidth.unwrap_or(0));
                out.push(MediaSegmentRef {
                    url: join(&self.base, &url),
                    byte_range: None,
                    seq,
                    duration: Some(dur as f64 / timescale as f64),
                });
                seq += 1;
            }
            return Ok(out);
        }

        if !self.segment_list_urls.is_empty() {
            return Ok(self
                .segment_list_urls
                .iter()
                .enumerate()
                .map(|(i, u)| MediaSegmentRef {
                    url: join(&self.base, u),
                    byte_range: None,
                    seq: i as u64 + 1,
                    duration: None,
                })
                .collect());
        }

        Err(RiptideError::Manifest(format!(
            "representation {:?} exposes no segments (only SegmentTemplate, SegmentList and \
             SegmentBase are understood)",
            self.id
        )))
    }

    pub fn init_url(&self) -> Option<String> {
        if let Some(tpl) = &self.init_template {
            let url = subst(
                tpl,
                &self.id,
                self.start_number.max(1),
                0,
                self.bandwidth.unwrap_or(0),
            );
            Some(join(&self.base, &url))
        } else {
            self.segment_list_init
                .as_deref()
                .map(|u| join(&self.base, u))
        }
    }
}

fn join(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    Url::parse(base)
        .and_then(|b| b.join(rel))
        .map(|u| u.to_string())
        .unwrap_or_else(|_| rel.to_string())
}

/// Substitute DASH URL template identifiers.
pub fn subst(template: &str, rep_id: &str, number: u64, time: u64, bandwidth: u64) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let mut token = String::new();
        let mut closed = false;
        for c2 in chars.by_ref() {
            if c2 == '$' {
                closed = true;
                break;
            }
            token.push(c2);
        }
        if !closed {
            out.push('$');
            out.push_str(&token);
            continue;
        }
        if token.is_empty() {
            // "$$" is an escaped dollar sign.
            out.push('$');
            continue;
        }
        let (name, fmt) = match token.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (token.as_str(), None),
        };
        let value = match name {
            "RepresentationID" => Some(rep_id.to_string()),
            "Number" => Some(fmt_num(number, fmt)),
            "Time" => Some(fmt_num(time, fmt)),
            "Bandwidth" => Some(fmt_num(bandwidth, fmt)),
            _ => None,
        };
        match value {
            Some(v) => out.push_str(&v),
            None => {
                out.push('$');
                out.push_str(&token);
                out.push('$');
            }
        }
    }
    out
}

/// `%05d`-style zero padding used inside DASH templates.
fn fmt_num(n: u64, fmt: Option<&str>) -> String {
    if let Some(f) = fmt {
        let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(w) = digits.parse::<usize>() {
            if w > 0 && w < 32 {
                return format!("{n:0>w$}");
            }
        }
    }
    n.to_string()
}

/// Parse an ISO-8601 duration such as `PT1H2M3.5S`.
pub fn parse_iso_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_prefix('P')?;
    let mut total = 0.0f64;
    let mut num = String::new();
    let mut in_time = false;
    for c in s.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' | '.' => num.push(c),
            _ => {
                let v: f64 = num.parse().ok()?;
                num.clear();
                total += match c {
                    'Y' => v * 365.0 * 86400.0,
                    'M' => {
                        if in_time {
                            v * 60.0
                        } else {
                            // Months have no fixed length; 30 days is the conventional reading.
                            v * 30.0 * 86400.0
                        }
                    }
                    'W' => v * 7.0 * 86400.0,
                    'D' => v * 86400.0,
                    'H' => v * 3600.0,
                    'S' => v,
                    _ => return None,
                };
            }
        }
    }
    if total > 0.0 {
        Some(total)
    } else {
        None
    }
}

fn local_name(e: &BytesStart<'_>) -> String {
    let raw = e.name();
    let bytes = raw.as_ref();
    match bytes.iter().position(|b| *b == b':') {
        Some(i) => String::from_utf8_lossy(&bytes[i + 1..]).to_string(),
        None => String::from_utf8_lossy(bytes).to_string(),
    }
}

fn attr_map(e: &BytesStart<'_>) -> Result<HashMap<String, String>> {
    let mut m = HashMap::new();
    for a in e.attributes() {
        let a = a.map_err(|err| RiptideError::Manifest(format!("bad XML attribute: {err}")))?;
        let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
        // Strip any namespace prefix from the attribute name too.
        let key = match key.split_once(':') {
            Some((_, k)) => k.to_string(),
            None => key,
        };
        let value = a
            .unescape_value()
            .map(|c| c.into_owned())
            .unwrap_or_default();
        m.insert(key, value);
    }
    Ok(m)
}

fn get<'a>(m: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    m.get(k).map(|s| s.as_str())
}

fn get_u64(m: &HashMap<String, String>, k: &str) -> Option<u64> {
    get(m, k).and_then(|v| v.trim().parse().ok())
}

fn get_u32(m: &HashMap<String, String>, k: &str) -> Option<u32> {
    get(m, k).and_then(|v| v.trim().parse().ok())
}

#[derive(Default, Clone)]
struct Frame {
    name: String,
    attrs: HashMap<String, String>,
    base_url: Option<String>,
    init_template: Option<String>,
    media_template: Option<String>,
    timescale: Option<u64>,
    duration: Option<u64>,
    start_number: Option<u64>,
    timeline: Vec<(Option<u64>, u64, i64)>,
    segment_list_init: Option<String>,
    segment_list_urls: Vec<String>,
}

/// Parse an MPD into a flat list of representations.
pub fn parse_mpd(xml: &str, manifest_url: &str) -> Result<(Vec<Rep>, Option<f64>)> {
    let mpd_base = {
        let u = Url::parse(manifest_url).map_err(|e| RiptideError::Manifest(e.to_string()))?;
        let mut b = u.clone();
        b.set_query(None);
        b.set_fragment(None);
        // Truncate to the last '/' so the result is unambiguously a *directory* base.
        // (`path_segments_mut().pop()` is not reliable here: joining a relative reference
        // against a base without a trailing slash would drop the last path segment.)
        let path = b.path().to_string();
        let dir = match path.rfind('/') {
            Some(i) => path[..=i].to_string(),
            None => "/".to_string(),
        };
        b.set_path(&dir);
        b.to_string()
    };

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<Frame> = Vec::new();
    let mut reps: Vec<Rep> = Vec::new();
    let mut mpd_duration: Option<f64> = None;
    let mut text = String::new();
    let mut buf = Vec::new();

    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| RiptideError::Manifest(format!("XML error: {e}")))?;
        match ev {
            Event::Start(e) => {
                text.clear();
                let name = local_name(&e);
                let attrs = attr_map(&e)?;
                if name == "MPD" {
                    mpd_duration =
                        get(&attrs, "mediaPresentationDuration").and_then(parse_iso_duration);
                }
                if name == "SegmentTemplate" {
                    // A Start form may carry a SegmentTimeline child.
                    stack.push(Frame {
                        name,
                        attrs,
                        ..Default::default()
                    });
                } else {
                    stack.push(Frame {
                        name,
                        attrs,
                        ..Default::default()
                    });
                }
            }
            Event::Empty(e) => {
                text.clear();
                let name = local_name(&e);
                let attrs = attr_map(&e)?;
                match name.as_str() {
                    "BaseURL" => {
                        if let Some(top) = stack.last_mut() {
                            top.base_url = get(&attrs, "base").map(|s| s.to_string());
                        }
                    }
                    "SegmentTemplate" => {
                        if let Some(top) = stack.last_mut() {
                            top.init_template =
                                get(&attrs, "initialization").map(|s| s.to_string());
                            top.media_template = get(&attrs, "media").map(|s| s.to_string());
                            top.timescale = get_u64(&attrs, "timescale");
                            top.duration = get_u64(&attrs, "duration");
                            top.start_number = get_u64(&attrs, "startNumber");
                        }
                    }
                    "Initialization" => {
                        if let Some(top) = stack.last_mut() {
                            top.segment_list_init = get(&attrs, "sourceURL").map(|s| s.to_string());
                        }
                    }
                    "SegmentURL" => {
                        if let Some(top) = stack.last_mut() {
                            if let Some(m) = get(&attrs, "media") {
                                top.segment_list_urls.push(m.to_string());
                            }
                        }
                    }
                    "S" => {
                        if let Some(top) = stack.last_mut() {
                            let t = get_u64(&attrs, "t");
                            let d = get_u64(&attrs, "d").unwrap_or(0);
                            let r: i64 = get(&attrs, "r").and_then(|v| v.parse().ok()).unwrap_or(0);
                            top.timeline.push((t, d, r));
                        }
                    }
                    "Representation" => {
                        reps.push(build_rep(&stack, &attrs, None, &mpd_base, mpd_duration));
                    }
                    "SegmentList" => {
                        if let Some(top) = stack.last_mut() {
                            if let Some(i) = get(&attrs, "initialization") {
                                top.segment_list_init = Some(i.to_string());
                            }
                        }
                    }
                    _ => {}
                }
                // Attributes on an Empty element are its complete content.
                if name == "SegmentTimeline" {
                    // nothing
                }
            }
            Event::Text(e) => {
                text.push_str(&e.unescape().map(|c| c.into_owned()).unwrap_or_default());
            }
            Event::End(e) => {
                let name = {
                    let raw = e.name();
                    let bytes = raw.as_ref();
                    match bytes.iter().position(|b| *b == b':') {
                        Some(i) => String::from_utf8_lossy(&bytes[i + 1..]).to_string(),
                        None => String::from_utf8_lossy(bytes).to_string(),
                    }
                };
                let frame = stack.pop();
                match name.as_str() {
                    "BaseURL" => {
                        if let (Some(f), Some(parent)) = (frame, stack.last_mut()) {
                            // A BaseURL with only text (the common case).
                            let value = if text.trim().is_empty() {
                                f.base_url.clone()
                            } else {
                                Some(text.trim().to_string())
                            };
                            if let Some(v) = value {
                                parent.base_url = Some(v);
                            }
                        }
                    }
                    "SegmentTimeline" => {
                        if let (Some(f), Some(parent)) = (frame, stack.last_mut()) {
                            parent.timeline = f.timeline;
                        }
                    }
                    "SegmentTemplate" => {
                        if let (Some(f), Some(parent)) = (frame, stack.last_mut()) {
                            parent.init_template =
                                get(&f.attrs, "initialization").map(|s| s.to_string());
                            parent.media_template = get(&f.attrs, "media").map(|s| s.to_string());
                            parent.timescale = get_u64(&f.attrs, "timescale").or(parent.timescale);
                            parent.duration = get_u64(&f.attrs, "duration");
                            parent.start_number = get_u64(&f.attrs, "startNumber");
                            if !f.timeline.is_empty() {
                                parent.timeline = f.timeline;
                            }
                        }
                    }
                    "SegmentList" => {
                        if let (Some(f), Some(parent)) = (frame, stack.last_mut()) {
                            parent.segment_list_init = f.segment_list_init;
                            parent.segment_list_urls = f.segment_list_urls;
                        }
                    }
                    "Representation" => {
                        if let Some(f) = frame {
                            reps.push(build_rep(
                                &stack,
                                &f.attrs,
                                Some(&f),
                                &mpd_base,
                                mpd_duration,
                            ));
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    if reps.is_empty() {
        return Err(RiptideError::Manifest(
            "MPD contains no <Representation> elements".into(),
        ));
    }
    Ok((reps, mpd_duration))
}

/// Flatten one `<Representation>` together with everything it inherits.
///
/// `own` is the representation's own XML frame (present when it had child elements such as
/// `SegmentTemplate`); ancestors come first so that a Representation-level definition wins
/// over an AdaptationSet-level one.
fn build_rep(
    stack: &[Frame],
    attrs: &HashMap<String, String>,
    own: Option<&Frame>,
    mpd_base: &str,
    mpd_duration: Option<f64>,
) -> Rep {
    let mut chain: Vec<&Frame> = stack.iter().collect();
    if let Some(o) = own {
        chain.push(o);
    }

    // Resolve the BaseURL chain outermost first.
    let mut base = mpd_base.to_string();
    for f in &chain {
        if let Some(b) = &f.base_url {
            base = join(&base, b);
        }
    }

    // Inherit from the nearest ancestor that defines each property.
    let find = |key: &str| -> Option<String> {
        for f in chain.iter().rev() {
            if let Some(v) = f.attrs.get(key) {
                return Some(v.clone());
            }
        }
        None
    };

    let mime = get(attrs, "mimeType")
        .map(|s| s.to_string())
        .or_else(|| find("mimeType"))
        .or_else(|| get(attrs, "contentType").map(|s| s.to_string()))
        .or_else(|| find("contentType").map(|s| format!("{s}/mp4")));

    // SegmentTemplate may live on the AdaptationSet (inherited) or on the Representation.
    let mut init_template = None;
    let mut media_template = None;
    let mut timescale = None;
    let mut duration = None;
    let mut start_number = None;
    let mut timeline: Vec<(Option<u64>, u64, i64)> = Vec::new();
    let mut segment_list_init = None;
    let mut segment_list_urls: Vec<String> = Vec::new();

    for f in &chain {
        if let Some(v) = &f.init_template {
            init_template = Some(v.clone());
        }
        if let Some(v) = &f.media_template {
            media_template = Some(v.clone());
        }
        if let Some(v) = f.timescale {
            timescale = Some(v);
        }
        if let Some(v) = f.duration {
            duration = Some(v);
        }
        if let Some(v) = f.start_number {
            start_number = Some(v);
        }
        if !f.timeline.is_empty() {
            timeline = f.timeline.clone();
        }
        if let Some(v) = &f.segment_list_init {
            segment_list_init = Some(v.clone());
        }
        if !f.segment_list_urls.is_empty() {
            segment_list_urls = f.segment_list_urls.clone();
        }
    }

    // The nearest enclosing Period's duration, else the MPD's.
    let period_duration = stack
        .iter()
        .rev()
        .find(|f| f.name == "Period")
        .and_then(|f| f.attrs.get("duration"))
        .and_then(|d| parse_iso_duration(d))
        .or(mpd_duration);

    Rep {
        id: get(attrs, "id").unwrap_or("0").to_string(),
        bandwidth: get_u64(attrs, "bandwidth"),
        width: get_u32(attrs, "width"),
        height: get_u32(attrs, "height"),
        codecs: get(attrs, "codecs").map(|s| s.to_string()),
        mime,
        base,
        init_template,
        media_template,
        timescale: timescale.unwrap_or(1),
        duration,
        start_number: start_number.unwrap_or(1),
        timeline,
        segment_list_init,
        segment_list_urls,
        period_duration,
    }
}

/// Pick a representation. `preference` is `best`, `worst`, or a height such as `720`.
pub fn select<'a>(reps: &'a [Rep], preference: Option<&str>) -> Option<&'a Rep> {
    if reps.is_empty() {
        return None;
    }
    let pref = preference.unwrap_or("best").trim().to_ascii_lowercase();
    let score = |r: &Rep| -> u64 {
        r.width.unwrap_or(0) as u64 * r.height.unwrap_or(0) as u64 * 1_000_000
            + r.bandwidth.unwrap_or(0)
    };

    if pref == "worst" {
        return reps.iter().min_by_key(|r| score(r));
    }
    if let Ok(want) = pref.parse::<u32>() {
        let mut c: Vec<&Rep> = reps.iter().filter(|r| r.height.is_some()).collect();
        if !c.is_empty() {
            c.sort_by_key(|r| r.height.unwrap());
            return c
                .iter()
                .filter(|r| r.height.unwrap() <= want)
                .max_by_key(|r| r.height.unwrap())
                .copied()
                .or_else(|| c.first().copied());
        }
    }
    reps.iter().max_by_key(|r| score(r))
}

/// Resolve a DASH manifest into a [`MediaSpec`], including the audio track when the MPD
/// carries one.
pub async fn resolve(engine: &mut Engine) -> Result<MediaSpec> {
    let manifest_url = engine
        .job()
        .resolved_url
        .clone()
        .unwrap_or_else(|| engine.job().url.clone());
    let bytes = engine.fetch_bytes(&manifest_url, 32 << 20).await?;
    let xml = String::from_utf8_lossy(&bytes).to_string();
    let (reps, _) = parse_mpd(&xml, &manifest_url)?;

    let preference = engine.job().quality.clone();
    let pref = preference.as_deref();

    let video_reps: Vec<Rep> = reps.iter().filter(|r| r.is_video()).cloned().collect();
    let audio_reps: Vec<Rep> = reps.iter().filter(|r| r.is_audio()).cloned().collect();

    // With no explicit mime types, fall back to whatever selects to the best resolution.
    let (video, audio) = if video_reps.is_empty() {
        tracing::warn!(
            "DASH manifest declares no video AdaptationSet; using the best representation"
        );
        (select(&reps, pref).cloned(), None)
    } else {
        (
            select(&video_reps, pref).cloned(),
            select(&audio_reps, Some("best")).cloned(),
        )
    };

    let video = video.ok_or_else(|| {
        RiptideError::Manifest("could not choose a video representation from the MPD".into())
    })?;

    let init = video.init_url();
    let segments = video.segments()?;
    if segments.is_empty() {
        return Err(RiptideError::Manifest(
            "selected DASH representation expands to zero segments".into(),
        ));
    }

    let mut spec = MediaSpec {
        manifest_url: manifest_url.clone(),
        variant_url: None,
        variant_label: Some(video.label()),
        init_url: init,
        segments,
        key: None,
        audio: None,
        container: video.container(),
        total_bytes: None,
    };

    if let Some(a) = audio {
        match a.segments() {
            Ok(segs) if !segs.is_empty() => {
                spec.audio = Some(Box::new(MediaSpec {
                    manifest_url: manifest_url.clone(),
                    variant_url: None,
                    variant_label: Some(a.label()),
                    init_url: a.init_url(),
                    segments: segs,
                    key: None,
                    audio: None,
                    container: a.container(),
                    total_bytes: None,
                }));
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "audio representation could not be expanded");
            }
        }
    }

    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT30S" type="static">
  <BaseURL>https://cdn.test/dash/</BaseURL>
  <Period duration="PT30S">
    <AdaptationSet mimeType="video/mp4" segmentAlignment="true">
      <Representation id="v360" bandwidth="800000" width="640" height="360" codecs="avc1.4d401e">
        <SegmentTemplate timescale="90000" duration="180000"
          initialization="init-$RepresentationID$.mp4"
          media="seg-$RepresentationID$-$Number%05d$.m4s" startNumber="1"/>
      </Representation>
      <Representation id="v1080" bandwidth="5000000" width="1920" height="1080" codecs="avc1.640028">
        <SegmentTemplate timescale="90000" duration="180000"
          initialization="init-$RepresentationID$.mp4"
          media="seg-$RepresentationID$-$Number%05d$.m4s" startNumber="1"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" lang="en">
      <Representation id="a1" bandwidth="128000" codecs="mp4a.40.2">
        <SegmentTemplate timescale="48000" duration="96000"
          initialization="init-$RepresentationID$.mp4"
          media="seg-$RepresentationID$-$Number%05d$.m4s" startNumber="1"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn template_substitution_including_padding() {
        let t = "seg-$RepresentationID$-$Number%05d$-$Bandwidth$-$Time$.m4s";
        assert_eq!(
            subst(t, "v1", 7, 630000, 800000),
            "seg-v1-00007-800000-630000.m4s"
        );
    }

    #[test]
    fn escaped_dollar_and_unknown_tokens() {
        assert_eq!(subst("a$$b", "r", 1, 0, 0), "a$b");
        assert_eq!(subst("$Unknown$", "r", 1, 0, 0), "$Unknown$");
        assert_eq!(subst("no-tokens", "r", 1, 0, 0), "no-tokens");
    }

    #[test]
    fn parses_multi_adaptation_set_mpd() {
        let (reps, dur) = parse_mpd(MPD, "https://cdn.test/dash/manifest.mpd").unwrap();
        assert_eq!(dur, Some(30.0));
        assert_eq!(reps.len(), 3);

        let video: Vec<Rep> = reps.iter().filter(|r| r.is_video()).cloned().collect();
        let audio: Vec<Rep> = reps.iter().filter(|r| r.is_audio()).cloned().collect();
        assert_eq!(video.len(), 2);
        assert_eq!(audio.len(), 1);

        let best = select(&video, None).unwrap();
        assert_eq!(best.id, "v1080");
        assert_eq!(best.label(), "1920x1080");
        assert_eq!(select(&video, Some("worst")).unwrap().id, "v360");
        assert_eq!(select(&video, Some("720")).unwrap().id, "v360");
        assert_eq!(select(&video, Some("1080")).unwrap().id, "v1080");
        assert_eq!(select(&video, Some("9999")).unwrap().id, "v1080");

        assert_eq!(
            best.init_url().as_deref(),
            Some("https://cdn.test/dash/init-v1080.mp4")
        );
    }

    #[test]
    fn duration_based_segments_are_counted_from_period_duration() {
        let (reps, _) = parse_mpd(MPD, "https://cdn.test/dash/manifest.mpd").unwrap();
        let v = reps.iter().find(|r| r.id == "v1080").unwrap();
        let segs = v.segments().unwrap();
        // 30s of media in 2s segments (180000 / 90000).
        assert_eq!(segs.len(), 15);
        assert_eq!(segs[0].url, "https://cdn.test/dash/seg-v1080-00001.m4s");
        assert_eq!(segs[14].url, "https://cdn.test/dash/seg-v1080-00015.m4s");
        assert_eq!(segs[0].duration, Some(2.0));
    }

    #[test]
    fn segment_timeline_with_repeat_and_runs_to_end_of_period() {
        let xml = r#"<MPD mediaPresentationDuration="PT12S"><Period>
          <AdaptationSet mimeType="video/mp4"><Representation id="v" bandwidth="1">
            <SegmentTemplate timescale="10" initialization="i.mp4" media="t$Time$.m4s" startNumber="1">
              <SegmentTimeline>
                <S t="0" d="20" r="2"/>
                <S d="30"/>
              </SegmentTimeline>
            </SegmentTemplate>
          </Representation></AdaptationSet>
        </Period></MPD>"#;
        let (reps, _) = parse_mpd(xml, "https://cdn.test/x.mpd").unwrap();
        let segs = reps[0].segments().unwrap();
        // 3 repeats of the first entry (t=0,20,40) then one more at t=60.
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].url, "https://cdn.test/t0.m4s");
        assert_eq!(segs[1].url, "https://cdn.test/t20.m4s");
        assert_eq!(segs[2].url, "https://cdn.test/t40.m4s");
        assert_eq!(segs[3].url, "https://cdn.test/t60.m4s");
        // Without an explicit @t, time continues from the previous entry.
        assert_eq!(segs[3].duration, Some(3.0));
    }

    #[test]
    fn negative_repeat_fills_the_period() {
        let xml = r#"<MPD mediaPresentationDuration="PT10S"><Period>
          <AdaptationSet mimeType="video/mp4"><Representation id="v">
            <SegmentTemplate timescale="1" initialization="i" media="$Time$" startNumber="1">
              <SegmentTimeline><S t="0" d="1" r="-1"/></SegmentTimeline>
            </SegmentTemplate>
          </Representation></AdaptationSet>
        </Period></MPD>"#;
        let (reps, _) = parse_mpd(xml, "https://cdn.test/x.mpd").unwrap();
        let segs = reps[0].segments().unwrap();
        assert_eq!(segs.len(), 10, "1s segments across a 10s period");
    }

    #[test]
    fn segment_list_is_used_when_present() {
        let xml = r#"<MPD mediaPresentationDuration="PT4S"><Period>
          <AdaptationSet mimeType="video/mp4"><Representation id="v">
            <SegmentList>
              <Initialization sourceURL="init.mp4"/>
              <SegmentURL media="s1.m4s"/>
              <SegmentURL media="s2.m4s"/>
            </SegmentList>
          </Representation></AdaptationSet>
        </Period></MPD>"#;
        let (reps, _) = parse_mpd(xml, "https://cdn.test/d/x.mpd").unwrap();
        let r = &reps[0];
        assert_eq!(r.segments().unwrap().len(), 2);
        assert_eq!(r.init_url().as_deref(), Some("https://cdn.test/d/init.mp4"));
        assert_eq!(r.segments().unwrap()[1].url, "https://cdn.test/d/s2.m4s");
    }

    #[test]
    fn nested_base_urls_resolve_in_order() {
        let xml = r#"<MPD mediaPresentationDuration="PT2S">
          <BaseURL>https://a.test/</BaseURL>
          <Period><BaseURL>p/</BaseURL>
          <AdaptationSet mimeType="video/mp4"><Representation id="v">
            <BaseURL>r/</BaseURL>
            <SegmentTemplate timescale="1" duration="1" initialization="i" media="$Number$"/>
          </Representation></AdaptationSet>
        </Period></MPD>"#;
        let (reps, _) = parse_mpd(xml, "https://orig.test/m.mpd").unwrap();
        assert_eq!(reps[0].segments().unwrap()[0].url, "https://a.test/p/r/1");
    }

    #[test]
    fn iso_durations() {
        assert_eq!(parse_iso_duration("PT30S"), Some(30.0));
        assert_eq!(parse_iso_duration("PT1H2M3S"), Some(3723.0));
        assert_eq!(parse_iso_duration("PT1.5S"), Some(1.5));
        assert_eq!(parse_iso_duration("PT0S"), None);
        assert_eq!(parse_iso_duration("bogus"), None);
    }

    #[test]
    fn a_manifest_without_representations_is_an_error() {
        assert!(parse_mpd("<MPD></MPD>", "https://x/y.mpd").is_err());
    }
}
