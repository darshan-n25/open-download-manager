use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, U[0])
    } else {
        format!("{:.2} {}", v, U[i])
    }
}

pub fn gen_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..8).map(|_| format!("{:02x}", rng.gen::<u8>())).collect()
}

/// Strip characters that are illegal in a filename on Windows or POSIX.
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "download".to_string()
    } else if trimmed.len() > 200 {
        trimmed[..200].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Derive a filename from a URL path, percent-decoding as IDM does.
pub fn filename_from_url(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let last = u.path_segments()?.next_back()?;
    if last.is_empty() {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(last)
        .decode_utf8_lossy()
        .to_string();
    Some(sanitize_filename(&decoded))
}

/// Parse `Content-Disposition` for a filename (RFC 6266), including `filename*`.
pub fn filename_from_disposition(v: &str) -> Option<String> {
    for part in v.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("filename*=") {
            // ext-value: charset'lang'pct-encoded
            let rest = rest.trim_matches('"');
            let after = rest.splitn(3, '\'').nth(2).unwrap_or(rest);
            let decoded = percent_encoding::percent_decode_str(after)
                .decode_utf8_lossy()
                .to_string();
            if !decoded.is_empty() {
                return Some(sanitize_filename(&decoded));
            }
        } else if let Some(rest) = p.strip_prefix("filename=") {
            let rest = rest.trim().trim_matches('"');
            if !rest.is_empty() {
                return Some(sanitize_filename(rest));
            }
        }
    }
    None
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024u64),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        'b' => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|f| (f * mult as f64) as u64)
}

/// Write a file atomically: write to a sibling temp file, fsync, then rename.
/// This is what makes resume state crash-safe — a half-written state file is never
/// observable.
pub fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp: PathBuf = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("state"),
        std::process::id()
    ));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// MIME → canonical extension. Ported from the table shipped inside IDM's browser
/// extension (`Aa` in background.js) and mirrored server-side by IDMFType.dat.
pub fn extension_from_mime(mime: &str) -> Option<&'static str> {
    let m = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    Some(match m.as_str() {
        "application/vnd.apple.mpegurl"
        | "application/x-mpegurl"
        | "audio/mpegurl"
        | "audio/x-mpegurl"
        | "video/mpegurl"
        | "video/x-mpegurl"
        | "application/octet-stream-m3u8" => "m3u8",
        "application/dash+xml" | "video/vnd.mpeg.dash.mpd" | "application/f4m+xml" => "mpd",
        "video/mp4" | "video/x-mp4" | "video/mpg4" | "video/x-mpg4" | "audio/mp4" => "mp4",
        "video/mp2t" | "audio/mp2t" => "ts",
        "video/webm" | "audio/webm" => "webm",
        "video/x-flv" | "video/flv" | "video/x-flash-video" | "flv-application/octet-stream" => {
            "flv"
        }
        "video/mpeg" | "video/x-mpeg" => "mpg",
        "video/x-msvideo" | "video/avi" | "video/msvideo" => "avi",
        "video/quicktime" => "mov",
        "video/x-ms-wmv" => "wmv",
        "video/x-ms-asf" => "asf",
        "audio/mpeg" | "audio/x-mpeg" | "ilm/tm" => "mp3",
        "audio/mp4a-latm" | "audio/mpeg4-generic" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/x-ms-wma" => "wma",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        "application/zip" | "application/x-zip" | "application/x-zip-compressed" => "zip",
        "application/x-7z-compressed" | "application/x-compress-7z" => "7z",
        "application/x-rar" | "application/x-rar-compressed" => "rar",
        "application/gzip" | "application/x-gzip" | "application/x-gzip-compressed" => "gz",
        "application/x-tar" | "application/x-gtar" => "tar",
        "application/x-bittorrent" | "application/torrent" => "torrent",
        "application/x-msi" | "application/x-msp" | "application/x-ole-storage" => "msi",
        "application/x-dosexec" | "application/x-msdos-program" | "application/x-sdlc" => "exe",
        "application/x-chrome-extension" => "crx",
        "application/x-shockwave-flash" => "swf",
        "application/x-silverlight-app" => "xap",
        "application/vnd.yt-ump" => "ump",
        "application/x-subrip" | "text/srt" => "srt",
        "text/vtt" => "vtt",
        "text/xml" | "application/ttml+xml" => "xml",
        "application/json" | "text/x-json" | "application/json+protobuf" => "json",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/tiff" => "tif",
        "image/vnd.microsoft.icon" | "image/x-icon" | "image/icon" => "ico",
        "text/css" => "css",
        "text/javascript"
        | "application/javascript"
        | "application/x-javascript"
        | "text/x-javascript" => "js",
        "text/html" => "html",
        "text/plain" => "txt",
        _ => return None,
    })
}

/// IDM-style download categories, used to auto-sort files into folders.
pub fn category_for_extension(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "mpg" | "mpeg" | "m4v" | "ts"
        | "3gp" | "ogv" | "rmvb" | "asf" => "Video",
        "mp3" | "m4a" | "wav" | "wma" | "ogg" | "aac" | "flac" | "opus" => "Audio",
        "zip" | "rar" | "7z" | "gz" | "tar" | "bz2" | "xz" | "iso" | "img" | "z" => "Compressed",
        "exe" | "msi" | "apk" | "deb" | "rpm" | "dmg" | "appimage" => "Programs",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "epub" | "odt"
        | "ods" => "Documents",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "tif" | "tiff" | "avif" => {
            "Images"
        }
        "torrent" => "Torrents",
        _ => "General",
    }
}

/// A wildcard pattern (`*`, `?`) compiled to a regex, matching IDM's `defexclist.txt`
/// and grabber include/exclude filter semantics.
pub fn wildcard_to_regex(pattern: &str) -> String {
    let mut out = String::from("(?i)^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c => {
                if "\\.+()[]{}^$|".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    out.push('$');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.50 MB");
    }

    #[test]
    fn filenames_are_sanitised() {
        assert_eq!(sanitize_filename("a/b:c*.mp4"), "a_b_c_.mp4");
        assert_eq!(sanitize_filename(""), "download");
    }

    #[test]
    fn filename_from_url_decodes() {
        assert_eq!(
            filename_from_url("https://x.test/a/my%20file.zip?q=1").unwrap(),
            "my file.zip"
        );
        assert!(filename_from_url("https://x.test/").is_none());
    }

    #[test]
    fn disposition_variants() {
        assert_eq!(
            filename_from_disposition("attachment; filename=\"report.pdf\"").unwrap(),
            "report.pdf"
        );
        assert_eq!(
            filename_from_disposition("attachment; filename*=UTF-8''%E2%82%AC%20rates.pdf")
                .unwrap(),
            "€ rates.pdf"
        );
    }

    #[test]
    fn mime_mapping_matches_idm_table() {
        assert_eq!(
            extension_from_mime("application/vnd.apple.mpegurl"),
            Some("m3u8")
        );
        assert_eq!(extension_from_mime("video/mp2t"), Some("ts"));
        assert_eq!(extension_from_mime("application/dash+xml"), Some("mpd"));
        assert_eq!(
            extension_from_mime("video/mp4; charset=binary"),
            Some("mp4")
        );
        assert_eq!(extension_from_mime("application/x-nonsense"), None);
    }

    #[test]
    fn categories_sort_like_idm() {
        assert_eq!(category_for_extension("mp4"), "Video");
        assert_eq!(category_for_extension("MP3"), "Audio");
        assert_eq!(category_for_extension("zip"), "Compressed");
        assert_eq!(category_for_extension("weird"), "General");
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("1M"), Some(1024 * 1024));
        assert_eq!(parse_size("512k"), Some(512 * 1024));
        assert_eq!(parse_size("2048"), Some(2048));
    }

    #[test]
    fn wildcards() {
        let re = regex::Regex::new(&wildcard_to_regex("http://*.example.com/*.bin")).unwrap();
        assert!(re.is_match("http://cdn.example.com/a/b.bin"));
        assert!(!re.is_match("https://cdn.example.com/a/b.bin"));
    }
}
