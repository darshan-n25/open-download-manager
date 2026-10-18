//! HLS/DASH capture and muxing.
//!
//! IDM's media pipeline is visible in its RTTI: `CM3UParser` with `MasterPlaylist`,
//! `MediaPlaylist`, `MediaSegment` and `MPLVariant` for HLS, `CMp4DashReader` for DASH,
//! plus a `-tmp_dashinit` temp file for the DASH initialisation segment. Riptide keeps
//! the same shape — parse the manifest into a flat segment plan, fetch init + segments,
//! then produce one output file.
//!
//! One deliberate improvement over IDM: when a manifest carries separate video and audio
//! renditions, Riptide downloads both and muxes them, instead of leaving the user with a
//! video file and a stray audio file ("IDM keeps downloading audio and video separately!"
//! is one of the most common complaints about it).

pub mod dash;
pub mod hls;
pub mod remux;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::{Engine, Fetcher};
use crate::error::{Result, RiptideError};
use crate::job::{JobKind, MediaKey, MediaSpec};

/// Entry point used by [`crate::engine::Engine::run`].
pub async fn download(engine: &mut Engine) -> Result<()> {
    let spec = match engine.job().kind {
        JobKind::Hls => hls::resolve(engine).await?,
        JobKind::Dash => dash::resolve(engine).await?,
        _ => return Err(RiptideError::other("not a media job")),
    };
    tracing::info!(
        segments = spec.segments.len(),
        container = %spec.container,
        audio = spec.audio.is_some(),
        "resolved media manifest"
    );

    engine.job_mut().media = Some(spec.clone());
    engine.job_mut().total = None;
    engine.job_mut().resumable = true;

    let out = engine.job().output.clone();
    let done = fetch_and_mux(engine, &spec, &out).await?;
    let job = engine.job_mut();
    job.total = Some(done);
    job.downloaded = done;
    Ok(())
}

/// Where per-segment part files live for a given output path.
pub fn part_dir_for(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_os_string();
    s.push(".segments");
    PathBuf::from(s)
}

/// Fetch every segment of `spec` (and its audio track) and produce `out`.
pub async fn fetch_and_mux(engine: &Engine, spec: &MediaSpec, out: &Path) -> Result<u64> {
    let dir = part_dir_for(out);
    tokio::fs::create_dir_all(&dir).await?;

    let counter = Arc::new(AtomicU64::new(0));
    let total_hint = spec.total_bytes;
    let progress = engine.progress_cb();
    let monitor_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(cb) = progress.clone() {
        let counter = counter.clone();
        let stop = monitor_stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(300)).await;
                cb(crate::engine::ProgressSample {
                    downloaded: counter.load(Ordering::SeqCst),
                    total: total_hint,
                    speed_bps: 0.0,
                });
            }
        });
    }

    let fetcher = engine.fetcher();
    let concurrency = engine.job().connections.clamp(1, 16);

    let video = fetch_track(&fetcher, spec, &dir.join("v"), concurrency, &counter).await?;
    let output = if let Some(audio) = &spec.audio {
        let a = fetch_track(&fetcher, audio, &dir.join("a"), concurrency, &counter).await?;
        remux::mux(&video, &a, out, &spec.container)?
    } else {
        remux::publish_single(&video, out, &spec.container)?
    };

    monitor_stop.store(true, Ordering::SeqCst);

    // Per-segment files have served their purpose; resuming is handled by the manifest
    // plan, so clear them out.
    let _ = tokio::fs::remove_dir_all(&dir).await;

    if let Some(cb) = progress {
        cb(crate::engine::ProgressSample {
            downloaded: output,
            total: Some(output),
            speed_bps: 0.0,
        })
    }
    Ok(output)
}

/// Download one rendition into `dir`, returning the concatenated intermediate file.
///
/// Idempotent: an already-present part file is reused, which makes an interrupted media
/// download resumable without re-fetching everything.
async fn fetch_track(
    fetcher: &Fetcher,
    spec: &MediaSpec,
    dir: &Path,
    concurrency: usize,
    counter: &Arc<AtomicU64>,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;

    // Resolve the content key once for the whole track.
    let key = resolve_key(fetcher, spec.key.as_ref()).await?;

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut tasks = Vec::new();

    if let Some(init) = &spec.init_url {
        let dest = dir.join("0000-init.bin");
        if dest.exists() {
            counter.fetch_add(
                std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
                Ordering::SeqCst,
            );
        } else {
            let f = fetcher.clone();
            let url = init.clone();
            let dest2 = dest.clone();
            let c = counter.clone();
            tasks.push(tokio::spawn(async move {
                let n = f.to_file(&url, &dest2).await?;
                c.fetch_add(n, Ordering::SeqCst);
                Ok::<(), RiptideError>(())
            }));
        }
    }

    for (i, seg) in spec.segments.iter().enumerate() {
        let dest = dir.join(format!("{:06}.bin", i + 1));
        if dest.exists() {
            counter.fetch_add(
                std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
                Ordering::SeqCst,
            );
            continue;
        }
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| RiptideError::other(e.to_string()))?;
        let f = fetcher.clone();
        let url = seg.url.clone();
        let range = seg.byte_range;
        let seq = seg.seq;
        let key = key.clone();
        let iv = spec.key.as_ref().and_then(|k| k.iv.clone());
        let c = counter.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            // 64 MiB is a generous ceiling for a single media segment.
            let mut data = f.to_bytes_range(&url, range, 64 << 20).await?;
            if let Some(k) = &key {
                data = decrypt_aes128_cbc(k, iv.as_deref(), seq, &data)?;
            }
            let n = data.len() as u64;
            tokio::fs::write(&dest, &data).await?;
            c.fetch_add(n, Ordering::SeqCst);
            Ok::<(), RiptideError>(())
        }));
    }

    for t in tasks {
        t.await
            .map_err(|e| RiptideError::other(format!("segment task panicked: {e}")))??;
    }

    // Concatenate init first, then the segments in manifest order.
    let mut order: Vec<PathBuf> = Vec::new();
    let init = dir.join("0000-init.bin");
    if init.exists() {
        order.push(init);
    }
    for i in 0..spec.segments.len() {
        let p = dir.join(format!("{:06}.bin", i + 1));
        if p.exists() {
            order.push(p);
        } else {
            return Err(RiptideError::other(format!(
                "segment {} of the manifest was never downloaded",
                i + 1
            )));
        }
    }
    let joined = dir.join("track.bin");
    remux::concat(&order, &joined)?;
    Ok(joined)
}

async fn resolve_key(fetcher: &Fetcher, key: Option<&MediaKey>) -> Result<Option<Vec<u8>>> {
    let Some(k) = key else { return Ok(None) };
    if !k.method.eq_ignore_ascii_case("AES-128") {
        return Err(RiptideError::Unsupported(format!(
            "HLS encryption method {} (only AES-128 is supported)",
            k.method
        )));
    }
    if let Some(bytes) = &k.key {
        return Ok(Some(bytes.clone()));
    }
    let bytes = fetcher.to_bytes(&k.uri, 1024).await?;
    if bytes.len() < 16 {
        return Err(RiptideError::Manifest(
            "AES-128 key is shorter than 16 bytes".into(),
        ));
    }
    Ok(Some(bytes[..16].to_vec()))
}

/// AES-128-CBC decryption, which is what `#EXT-X-KEY:METHOD=AES-128` uses.
/// The IV defaults to the media sequence number as a 16-byte big-endian value.
pub fn decrypt_aes128_cbc(
    key: &[u8],
    iv: Option<&[u8]>,
    sequence: u64,
    data: &[u8],
) -> Result<Vec<u8>> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    if key.len() != 16 {
        return Err(RiptideError::Manifest(
            "AES-128 key must be 16 bytes".into(),
        ));
    }
    let mut iv_bytes = [0u8; 16];
    match iv {
        Some(v) if v.len() == 16 => iv_bytes.copy_from_slice(v),
        Some(v) => {
            return Err(RiptideError::Manifest(format!(
                "IV must be 16 bytes, got {}",
                v.len()
            )))
        }
        // The default IV is the media sequence number as a full 128-bit big-endian value,
        // i.e. eight zero bytes followed by the 64-bit sequence.
        None => iv_bytes[8..].copy_from_slice(&sequence.to_be_bytes()),
    }

    type Dec = cbc::Decryptor<aes::Aes128>;
    let mut buf = data.to_vec();
    let cipher = Dec::new(key.into(), (&iv_bytes).into());
    let plain = cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| RiptideError::Manifest(format!("AES-128-CBC padding error: {e}")))?;
    Ok(plain.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes128_cbc_roundtrip() {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Enc = cbc::Encryptor<aes::Aes128>;

        let key = [7u8; 16];
        let iv = [9u8; 16];
        let plaintext = b"the quick brown fox jumps over the lazy dog!!";

        let mut buf = vec![0u8; plaintext.len() + 16];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let ct = Enc::new((&key).into(), (&iv).into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .unwrap()
            .to_vec();

        let back = decrypt_aes128_cbc(&key, Some(&iv), 0, &ct).unwrap();
        assert_eq!(&back, plaintext);
    }

    #[test]
    fn iv_defaults_to_sequence_number() {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Enc = cbc::Encryptor<aes::Aes128>;

        let key = [1u8; 16];
        let seq = 42u64;
        let mut iv = [0u8; 16];
        iv[8..].copy_from_slice(&seq.to_be_bytes());
        let plaintext = b"0123456789abcdef";

        let mut buf = vec![0u8; 32];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let ct = Enc::new((&key).into(), (&iv).into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .unwrap()
            .to_vec();

        let back = decrypt_aes128_cbc(&key, None, seq, &ct).unwrap();
        assert_eq!(&back, plaintext);
    }

    #[test]
    fn wrong_iv_length_is_rejected() {
        let err = decrypt_aes128_cbc(&[0u8; 16], Some(&[0u8; 8]), 0, &[0u8; 16]).unwrap_err();
        assert!(matches!(err, RiptideError::Manifest(_)));
    }

    #[test]
    fn a_wrong_length_key_is_rejected() {
        assert!(decrypt_aes128_cbc(&[0u8; 12], None, 0, &[0u8; 16]).is_err());
    }

    /// The sequence-derived IV must be the 128-bit big-endian number, not the bare 64-bit
    /// `to_be_bytes()` (which is only 8 bytes and would panic on a fixed-size copy).
    #[test]
    fn sequence_iv_is_the_full_128_bit_value() {
        let mut iv_bytes = [0u8; 16];
        iv_bytes[8..].copy_from_slice(&1u64.to_be_bytes());
        assert_eq!(
            iv_bytes,
            [
                0, 0, 0, 0, 0, 0, 0, 0, //
                0, 0, 0, 0, 0, 0, 0, 1
            ]
        );
        // And it round-trips through the real function.
        let key = [3u8; 16];
        let enc = encrypt_for_test(&key, &iv_bytes, b"payload");
        assert_eq!(decrypt_aes128_cbc(&key, None, 1, &enc).unwrap(), b"payload");
    }

    #[cfg(test)]
    fn encrypt_for_test(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Vec<u8> {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Enc = cbc::Encryptor<aes::Aes128>;
        let mut buf = vec![0u8; plaintext.len() + 16];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        Enc::new(key.into(), iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .unwrap()
            .to_vec()
    }

    #[test]
    fn part_dir_is_derived_from_output() {
        assert_eq!(
            part_dir_for(Path::new("/tmp/movie.mp4")),
            PathBuf::from("/tmp/movie.mp4.segments")
        );
    }
}
