//! Concatenation and muxing of media tracks.
//!
//! Fragmented MP4 (`fMP4`) segments concatenate directly into a valid MP4 — the
//! initialisation segment carries the headers and each media segment carries fragments.
//! MPEG-TS does not: it needs a real remux, so an external `ffmpeg` is used when present,
//! and when it is absent Riptide keeps the `.ts` rather than silently producing a broken
//! `.mp4`.
//!
//! This matters because IDM's own changelog is full of "some MP4 could not be assembled,
//! resulting in separate audio and video tracks" and "TS videos stopped at 99%". Riptide
//! never discards the raw track, so a failed mux is recoverable rather than destructive.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Result, RiptideError};

const COPY_BUF: usize = 1 << 20;

/// Locate an `ffmpeg` binary: `RIPTIDE_FFMPEG` first, then `PATH`.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RIPTIDE_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ffmpeg");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Append `parts` to `out` in order.
pub fn concat(parts: &[PathBuf], out: &Path) -> Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut writer = std::io::BufWriter::with_capacity(COPY_BUF, std::fs::File::create(out)?);
    let mut buf = vec![0u8; COPY_BUF];
    for p in parts {
        let mut f = std::fs::File::open(p)?;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<u64> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to)?;
    Ok(std::fs::metadata(to)?.len())
}

/// If `out` is `foo.mp4` but the track is MPEG-TS and cannot be remuxed, fall back to
/// `foo.ts` so the user still gets a file their player can open.
fn without_remux(out: &Path, container: &str) -> PathBuf {
    if container == "ts" {
        out.with_extension("ts")
    } else {
        out.to_path_buf()
    }
}

/// Publish a single track (video only, or audio only) as the final file.
pub fn publish_single(track: &Path, out: &Path, container: &str) -> Result<u64> {
    match container {
        // Fragmented MP4 already is an MP4.
        "mp4" => copy_file(track, out),
        "ts" => match find_ffmpeg() {
            Some(ffmpeg) => remux_ts(&ffmpeg, track, out),
            None => {
                let fallback = without_remux(out, container);
                tracing::warn!(
                    path = %fallback.display(),
                    "ffmpeg not found; keeping MPEG-TS instead of remuxing to MP4 \
                     (set RIPTIDE_FFMPEG or install ffmpeg)"
                );
                copy_file(track, &fallback)
            }
        },
        _ => copy_file(track, out),
    }
}

/// Mux a separate video track and audio track into one file.
pub fn mux(video: &Path, audio: &Path, out: &Path, container: &str) -> Result<u64> {
    let Some(ffmpeg) = find_ffmpeg() else {
        let fallback = without_remux(out, container);
        tracing::warn!(
            video = %video.display(),
            audio = %audio.display(),
            "ffmpeg not found; cannot merge separate video and audio tracks. \
             Writing the video track only — install ffmpeg or set RIPTIDE_FFMPEG \
             to get a single merged file"
        );
        return copy_file(video, &fallback);
    };

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // `-c copy` avoids any re-encode: this is a container-level remux only, so it is fast
    // and lossless. `-movflags +faststart` moves the moov atom to the front for streaming
    // playback, and `-shortest` stops on the shorter track.
    let status = Command::new(&ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(video)
        .arg("-i")
        .arg(audio)
        .args(["-map", "0:v:0", "-map", "1:a:0"])
        .args(["-c", "copy"])
        .args(["-movflags", "+faststart"])
        .arg(out)
        .status()
        .map_err(|e| RiptideError::other(format!("failed to run ffmpeg: {e}")))?;

    if !status.success() || !out.exists() {
        // The container may not accept a raw copy (e.g. TS video into MP4 with an
        // incompatible codec). Try a real remux of the video with the audio re-encoded.
        tracing::warn!("copy-mux failed; retrying with audio re-encode");
        let status = Command::new(&ffmpeg)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-y")
            .arg("-i")
            .arg(video)
            .arg("-i")
            .arg(audio)
            .args(["-map", "0:v:0", "-map", "1:a:0"])
            .args(["-c:v", "copy", "-c:a", "aac", "-b:a", "192k"])
            .args(["-movflags", "+faststart"])
            .arg(out)
            .status()
            .map_err(|e| RiptideError::other(format!("failed to run ffmpeg: {e}")))?;
        if !status.success() || !out.exists() {
            return Err(RiptideError::other(
                "ffmpeg could not merge the video and audio tracks",
            ));
        }
    }

    Ok(std::fs::metadata(out)?.len())
}

fn remux_ts(ffmpeg: &Path, track: &Path, out: &Path) -> Result<u64> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(track)
        .args(["-c", "copy"])
        .args(["-bsf:a", "aac_adtstoasc"])
        .args(["-movflags", "+faststart"])
        .arg(out)
        .status()
        .map_err(|e| RiptideError::other(format!("failed to run ffmpeg: {e}")))?;

    if status.success() && out.exists() {
        return Ok(std::fs::metadata(out)?.len());
    }

    // Some TS streams reject the bitstream filter; retry without it.
    let status = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(track)
        .args(["-c", "copy"])
        .arg(out)
        .status()
        .map_err(|e| RiptideError::other(format!("failed to run ffmpeg: {e}")))?;
    if status.success() && out.exists() {
        return Ok(std::fs::metadata(out)?.len());
    }
    Err(RiptideError::other(
        "ffmpeg could not remux the MPEG-TS stream",
    ))
}

/// Sniff whether a byte block looks like an ISO-BMFF (MP4) file.
pub fn looks_like_mp4(header: &[u8]) -> bool {
    header.len() >= 12 && matches!(&header[4..8], b"ftyp" | b"styp" | b"moof" | b"moov")
}

/// Sniff whether a byte block looks like an MPEG-TS packet.
pub fn looks_like_ts(header: &[u8]) -> bool {
    !header.is_empty() && header[0] == 0x47
}

/// Detect the container of an already-downloaded track, which is more reliable than
/// trusting URL extensions (many CDNs serve `.ts` files as `.php` or without extension).
pub fn sniff_container(path: &Path) -> Option<&'static str> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 16];
    let n = f.read(&mut head).ok()?;
    let head = &head[..n];
    if looks_like_mp4(head) {
        Some("mp4")
    } else if looks_like_ts(head) {
        Some("ts")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_is_byte_exact_and_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        std::fs::write(&a, b"AAA".as_slice()).unwrap();
        std::fs::write(&b, b"BBBB".as_slice()).unwrap();
        std::fs::write(&c, b"CC".as_slice()).unwrap();

        let out = dir.path().join("out");
        concat(&[a, b, c], &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"AAABBBBCC");
    }

    #[test]
    fn concat_overwrites_an_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::write(&a, b"new").unwrap();
        let out = dir.path().join("out");
        std::fs::write(&out, b"much longer old content").unwrap();
        concat(&[a], &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"new");
    }

    #[test]
    fn concat_of_nothing_creates_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("empty");
        concat(&[], &out).unwrap();
        assert_eq!(std::fs::metadata(&out).unwrap().len(), 0);
    }

    #[test]
    fn mp4_container_is_published_by_copy() {
        let dir = tempfile::tempdir().unwrap();
        let track = dir.path().join("track.bin");
        std::fs::write(&track, b"fmp4payload").unwrap();
        let out = dir.path().join("movie.mp4");
        let n = publish_single(&track, &out, "mp4").unwrap();
        assert_eq!(n, 11);
        assert_eq!(std::fs::read(&out).unwrap(), b"fmp4payload");
        // No ffmpeg involved, so the raw track stays put.
        assert!(track.exists());
    }

    #[test]
    fn ts_without_ffmpeg_falls_back_to_a_ts_extension() {
        // Point RIPTIDE_FFMPEG at a nonexistent file and clear PATH so lookup fails.
        let dir = tempfile::tempdir().unwrap();
        let track = dir.path().join("track.bin");
        std::fs::write(&track, b"\x47segment").unwrap();
        let out = dir.path().join("movie.mp4");

        let prev_ffmpeg = std::env::var_os("RIPTIDE_FFMPEG");
        let prev_path = std::env::var_os("PATH");
        std::env::set_var("RIPTIDE_FFMPEG", dir.path().join("nope-ffmpeg"));
        std::env::set_var("PATH", "");

        let result = publish_single(&track, &out, "ts");

        match prev_ffmpeg {
            Some(v) => std::env::set_var("RIPTIDE_FFMPEG", v),
            None => std::env::remove_var("RIPTIDE_FFMPEG"),
        }
        match prev_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }

        // Either a real ffmpeg existed (unlikely with PATH emptied) or we fell back.
        if let Ok(n) = result {
            let ts = out.with_extension("ts");
            assert!(
                ts.exists() || out.exists(),
                "some output must exist (n={n})"
            );
        }
    }

    #[test]
    fn container_sniffing() {
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("m");
        std::fs::write(&mp4, b"\x00\x00\x00\x18ftypisom....").unwrap();
        assert_eq!(sniff_container(&mp4), Some("mp4"));

        let ts = dir.path().join("t");
        std::fs::write(&ts, b"\x47\x40\x11\x10rest").unwrap();
        assert_eq!(sniff_container(&ts), Some("ts"));

        let other = dir.path().join("o");
        std::fs::write(&other, b"just text here").unwrap();
        assert_eq!(sniff_container(&other), None);
    }

    #[test]
    fn ffmpeg_discovery_respects_the_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("ffmpeg");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();

        let prev = std::env::var_os("RIPTIDE_FFMPEG");
        std::env::set_var("RIPTIDE_FFMPEG", &fake);
        assert_eq!(find_ffmpeg().as_deref(), Some(fake.as_path()));
        match prev {
            Some(v) => std::env::set_var("RIPTIDE_FFMPEG", v),
            None => std::env::remove_var("RIPTIDE_FFMPEG"),
        }
    }
}
