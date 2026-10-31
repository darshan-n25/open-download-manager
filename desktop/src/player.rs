//! Watching a torrent while it downloads, in a player window of its own.
//!
//! Decoding is FFmpeg's job — the same library uTorrent's own streamer ships — so
//! this module only moves bytes around: it points FFmpeg at the torrent's localhost
//! URL, takes raw RGBA frames from its stdout and hands them to an egui texture, and
//! lets a second FFmpeg turn the audio track into PCM for the sound card.
//!
//! Nothing is written to a temporary file, and playback starts as soon as the
//! stream has buffered enough — not when the download finishes. The picture fills
//! its own window ([`Player::ui`] is drawn by the caller in a child viewport), with
//! the transport controls along the bottom the way a desktop media player has them.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::player::audio::AudioProc;

/// Every frame is scaled to this. A 4K film decoded to full RGBA is 33 MB per
/// frame, and the picture is scaled up to the window anyway, so 960x540 is plenty
/// and keeps the copy out of FFmpeg cheap enough to stay ahead of playback.
pub const WIDTH: usize = 960;
pub const HEIGHT: usize = 540;

/// How far ahead of the picture the audio may buffer, in samples. Small on
/// purpose: it is the slack that makes pausing the sound immediate.
#[cfg(any(windows, target_os = "macos"))]
const AUDIO_BUFFER: usize = 48_000 * 2 / 2;

// ---------------------------------------------------------------------------
// Finding FFmpeg
// ---------------------------------------------------------------------------

static FFMPEG: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// The FFmpeg executable to run: `ODM_FFMPEG` if it is set, then one shipped next
/// to this program, then whatever is on `PATH`.
pub fn ffmpeg() -> Result<PathBuf, String> {
    FFMPEG
        .get_or_init(|| {
            if let Ok(path) = std::env::var("ODM_FFMPEG") {
                let path = PathBuf::from(path);
                if path.is_file() {
                    return Ok(path);
                }
            }
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    for name in ["ffmpeg.exe", "ffmpeg"] {
                        let candidate = dir.join(name);
                        if candidate.is_file() {
                            return Ok(candidate);
                        }
                    }
                }
            }
            let on_path = PathBuf::from(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            match Command::new(&on_path)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Ok(s) if s.success() => Ok(on_path),
                _ => Err(
                    "FFmpeg was not found. Install it, put ffmpeg next to this program, \
                     or point ODM_FFMPEG at it."
                        .into(),
                ),
            }
        })
        .clone()
}

// ---------------------------------------------------------------------------
// The player
// ---------------------------------------------------------------------------

pub struct Player {
    title: String,
    source: String,
    /// Seconds into the stream that the current FFmpeg run started at.
    base: f64,
    /// When the current run started. `None` while paused.
    anchor: Option<Instant>,
    paused: Arc<AtomicBool>,
    /// 0–100. The audio callback reads it, the interface writes it.
    volume: Arc<AtomicU32>,
    /// What the sound was before it was muted.
    last_volume: u32,
    video: Option<VideoPipe>,
    audio: Option<AudioProc>,
    texture: Option<egui::TextureHandle>,
    /// The film's length, once the demuxer has reported it. `None` for a container
    /// that carries no duration (a live stream) and until the stream has opened.
    duration: Option<f64>,
    /// Where the scrubber is being dragged to, before the seek happens.
    scrub_to: Option<f64>,
    /// FFmpeg closed the stream: the film reached its end, or the torrent stalled.
    pub ended: bool,
    pub error: Option<String>,
}

impl Player {
    /// Open `source` — a localhost stream URL or a file — from the beginning.
    pub fn open(title: &str, source: &str) -> Result<Player, String> {
        ffmpeg()?;
        let mut player = Player {
            title: title.to_string(),
            source: source.to_string(),
            base: 0.0,
            anchor: Some(Instant::now()),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(100)),
            last_volume: 100,
            video: None,
            audio: None,
            texture: None,
            duration: None,
            scrub_to: None,
            ended: false,
            error: None,
        };
        player.start_at(0.0)?;
        Ok(player)
    }

    fn start_at(&mut self, at: f64) -> Result<(), String> {
        let ffmpeg = ffmpeg()?;
        self.stop_children();
        self.base = at.max(0.0);
        self.scrub_to = None;
        // A seek while playing keeps the clock running; a seek while paused (or
        // after the film ended) leaves it stopped where the picture is.
        self.anchor = if self.paused.load(Ordering::Relaxed) {
            None
        } else {
            Some(Instant::now())
        };
        self.ended = false;
        self.error = None;
        self.video = Some(VideoPipe::spawn(&ffmpeg, &self.source, self.base)?);
        // Sound is a bonus, not a requirement: a torrent with no audio track, or a
        // machine with no output device, should still play the picture. No sound
        // card, or a track in a format FFmpeg cannot decode, is not an error the
        // user must see.
        if let Ok(audio) = AudioProc::spawn(
            &ffmpeg,
            &self.source,
            self.base,
            self.paused.clone(),
            self.volume.clone(),
        ) {
            self.audio = Some(audio);
        }
        Ok(())
    }

    /// The name shown in the player window's title bar.
    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn toggle_pause(&mut self) {
        if self.ended {
            // The picture is the last frame: pressing play starts the film again
            // rather than leaving a dead frame on screen.
            self.seek_to(0.0);
            return;
        }
        if self.paused.load(Ordering::Relaxed) {
            self.anchor = Some(Instant::now());
            self.paused.store(false, Ordering::Relaxed);
        } else {
            // Freeze the clock first, so the position shown is the one that resumes.
            self.base = self.position();
            self.anchor = None;
            self.paused.store(true, Ordering::Relaxed);
        }
    }

    /// Where playback is, in seconds from the start of the file.
    pub fn position(&self) -> f64 {
        match self.anchor {
            Some(at) => self.base + at.elapsed().as_secs_f64(),
            None => self.base,
        }
    }

    /// Jump to an absolute position — what a scrubber and a seek button both do.
    pub fn seek_to(&mut self, at: f64) {
        let target = at.max(0.0);
        if let Err(e) = self.start_at(target) {
            self.error = Some(e);
        }
    }

    /// Jump `delta` seconds forward (or back).
    pub fn skip(&mut self, delta: f64) {
        self.seek_to(self.position() + delta);
    }

    pub fn volume(&self) -> u32 {
        self.volume.load(Ordering::Relaxed)
    }

    pub fn set_volume(&mut self, volume: u32) {
        let volume = volume.min(100);
        self.volume.store(volume, Ordering::Relaxed);
        if volume > 0 {
            self.last_volume = volume;
        }
    }

    pub fn toggle_mute(&mut self) {
        if self.volume() == 0 {
            self.set_volume(self.last_volume.max(1));
        } else {
            self.set_volume(0);
        }
    }

    fn stop_children(&mut self) {
        if let Some(mut v) = self.video.take() {
            v.stop();
        }
        if let Some(mut a) = self.audio.take() {
            a.stop();
        }
    }

    /// Take whatever the decoder has produced since the last frame and upload it.
    ///
    /// Called once per repaint. Frames that arrived while the interface was busy are
    /// dropped in favour of the newest: a player that falls behind should skip
    /// ahead, not play an ever-growing backlog.
    pub fn pump(&mut self, ctx: &egui::Context) {
        let (newest, duration, died) = {
            let Some(video) = self.video.as_ref() else {
                return;
            };
            let mut newest: Option<Vec<u8>> = None;
            let mut died = false;
            loop {
                match video.frames.try_recv() {
                    Ok(frame) => newest = Some(frame),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // The decoder stopped: the film ended, the torrent stalled,
                        // or FFmpeg failed. Freeze the clock on the last frame rather
                        // than letting it run past the end of the picture.
                        died = true;
                        break;
                    }
                }
            }
            let mut duration = None;
            while let Ok(d) = video.duration.try_recv() {
                if d > 0.0 {
                    duration = Some(d);
                }
            }
            (newest, duration, died)
        };

        if let Some(d) = duration {
            self.duration = Some(d);
        }
        if died {
            self.ended = true;
            self.anchor = None;
            // A decoder that dies before the first second of picture has failed;
            // saying only "buffering…" would hide the reason FFmpeg printed.
            if self.error.is_none() && self.position() < 1.0 {
                self.error = self.video.as_ref().and_then(VideoPipe::last_error);
            }
        }
        let Some(frame) = newest else { return };
        let image = egui::ColorImage::from_rgba_unmultiplied([WIDTH, HEIGHT], &frame);
        match &mut self.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("odm-video", image, egui::TextureOptions::LINEAR));
            }
        }
    }

    /// Draw the player in its own window and return true when it should close.
    ///
    /// The layout is a media player's: the picture takes the window, the transport
    /// controls sit along the bottom, and the picture itself is clickable.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let ctx = ui.ctx().clone();
        let mut close = false;
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));

        // A player without Space and the arrow keys feels broken.
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.toggle_pause();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
            self.skip(-10.0);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
            self.skip(10.0);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::M)) {
            self.toggle_mute();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }

        // ----- the picture -------------------------------------------------
        let controls = 74.0;
        let available = ui.available_size();
        let area_size = egui::vec2(available.x, (available.y - controls).max(80.0));
        let aspect = WIDTH as f32 / HEIGHT as f32;
        let mut picture = egui::vec2(area_size.x, area_size.x / aspect);
        if picture.y > area_size.y {
            picture = egui::vec2(area_size.y * aspect, area_size.y);
        }

        let (area, _) = ui.allocate_exact_size(area_size, egui::Sense::hover());
        let picture_rect = egui::Rect::from_center_size(area.center(), picture);

        match &self.texture {
            Some(texture) => {
                let clicked = ui
                    .put(
                        picture_rect,
                        egui::Image::new(texture)
                            .fit_to_exact_size(picture)
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_text("Click to pause · double-click for full screen");
                if clicked.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
                } else if clicked.clicked() {
                    self.toggle_pause();
                }
            }
            None => {
                ui.painter()
                    .rect_filled(picture_rect, 2.0, egui::Color32::BLACK);
                let (text, color) = match (&self.error, self.ended) {
                    (Some(e), _) => (e.clone(), egui::Color32::from_rgb(248, 113, 113)),
                    (None, true) => ("the stream ended".to_string(), egui::Color32::GRAY),
                    (None, false) => ("buffering…".to_string(), egui::Color32::GRAY),
                };
                ui.painter().text(
                    picture_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(15.0),
                    color,
                );
            }
        }

        // ----- the scrubber and the clock ----------------------------------
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let position = self.scrub_to.unwrap_or_else(|| self.position());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clock = match self.duration {
                    Some(total) => format!("{} / {}", fmt_clock(position), fmt_clock(total)),
                    None => fmt_clock(position),
                };
                ui.label(
                    egui::RichText::new(clock)
                        .monospace()
                        .size(12.5)
                        .color(egui::Color32::from_rgb(203, 213, 225)),
                );
                if let Some(total) = self.duration.filter(|d| *d > 0.0) {
                    let width = ui.available_width().max(80.0);
                    let mut value = position.clamp(0.0, total);
                    let response = ui.add_sized(
                        [width, 18.0],
                        egui::Slider::new(&mut value, 0.0..=total).show_value(false),
                    );
                    if response.dragged() || response.drag_started() {
                        // Show where the thumb is, but do not restart FFmpeg on
                        // every frame of the drag: it seeks when the drag ends.
                        self.scrub_to = Some(value);
                    }
                    if response.drag_stopped() {
                        self.scrub_to = None;
                        self.seek_to(value);
                    }
                    // A click on the track is a seek too, not only a drag: that is
                    // how every media player behaves, and how a viewer expects to
                    // jump to a point they can see.
                    if response.clicked() {
                        self.scrub_to = None;
                        self.seek_to(value);
                    }
                }
            });
        });

        // ----- the transport controls --------------------------------------
        ui.horizontal(|ui| {
            if icon_button(ui, "⏮", "Restart").clicked() {
                self.seek_to(0.0);
            }
            if icon_button(ui, "⏪", "Back 10 seconds").clicked() {
                self.skip(-10.0);
            }
            // A film that has run out is not "playing": the button offers to start
            // it again rather than to pause a finished picture.
            let paused = self.paused() || self.ended;
            let play =
                egui::Button::new(egui::RichText::new(if paused { "▶" } else { "⏸" }).size(20.0))
                    .min_size(egui::vec2(48.0, 30.0));
            if ui
                .add(play)
                .on_hover_text(if paused {
                    "Play (Space)"
                } else {
                    "Pause (Space)"
                })
                .clicked()
            {
                self.toggle_pause();
            }
            if icon_button(ui, "⏩", "Forward 10 seconds").clicked() {
                self.skip(10.0);
            }
            if icon_button(ui, "⏭", "Forward a minute").clicked() {
                self.skip(60.0);
            }

            ui.separator();

            let volume = self.volume();
            let mute = if volume == 0 { "🔇" } else { "🔊" };
            if icon_button(ui, mute, "Mute (M)").clicked() {
                self.toggle_mute();
            }
            let mut level = volume as f32;
            let slider = egui::Slider::new(&mut level, 0.0..=100.0).show_value(false);
            if ui
                .add_sized([96.0, 18.0], slider)
                .on_hover_text(format!("Volume {volume}%"))
                .changed()
            {
                self.set_volume(level as u32);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, "✖", "Close the player").clicked() {
                    close = true;
                }
                if icon_button(ui, "⛶", "Full screen (F)").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
                }
                ui.label(
                    egui::RichText::new(&self.title)
                        .size(11.5)
                        .color(egui::Color32::from_rgb(148, 163, 184)),
                );
            });
        });

        close
    }
}

/// One transport button: a symbol the bundled fonts actually have, and a tooltip,
/// because a symbol alone is a guess the first time.
fn icon_button(ui: &mut egui::Ui, symbol: &str, tooltip: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(symbol).size(16.0)).min_size(egui::vec2(34.0, 28.0)),
    )
    .on_hover_text(tooltip)
}

/// Is this an FFmpeg line worth showing a user? The banner lines name the streams;
/// the complaint lines say why nothing is playing.
fn is_complaint(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    line.starts_with('[')
        || line.contains("error")
        || line.contains("Error")
        || line.contains("failed")
        || line.contains("Invalid")
        || line.contains("No such")
        || line.contains("Server returned")
}

/// `Duration: 00:01:23.45,` — the only place a stream's length is stated up front.
fn parse_duration(line: &str) -> Option<f64> {
    let rest = line.split("Duration: ").nth(1)?;
    let stamp = rest.split(',').next()?.trim();
    let mut parts = stamp.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    (total > 0.0).then_some(total)
}

/// `12:34`, or `1:02:34` once there is an hour to show.
fn fmt_clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m:02}:{sec:02}")
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.paused.store(true, Ordering::Relaxed);
        self.stop_children();
    }
}

// ---------------------------------------------------------------------------
// The video pipe
// ---------------------------------------------------------------------------

struct VideoPipe {
    child: Child,
    frames: Receiver<Vec<u8>>,
    /// The film's length, once FFmpeg has opened the input and printed it.
    duration: Receiver<f64>,
    /// The last thing FFmpeg said, for when the decoder dies without a picture.
    tail: Arc<std::sync::Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
}

impl VideoPipe {
    /// What FFmpeg complained about, if anything — `None` when it exited quietly,
    /// which is what happens at the end of a film.
    fn last_error(&self) -> Option<String> {
        self.tail.lock().ok().and_then(|t| t.clone())
    }

    fn spawn(ffmpeg: &std::path::Path, source: &str, at: f64) -> Result<VideoPipe, String> {
        let mut cmd = Command::new(ffmpeg);
        // `info` (not `error`) is what makes FFmpeg print the input's `Duration:` on
        // stderr, which is the only place a stream's length can be learned before
        // the film has played. `-nostats` keeps the per-frame progress lines out.
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "info", "-nostats"]);
        // `-re` makes FFmpeg read at the file's natural rate. Without it the decoder
        // would hand us the whole film in a second and playback would race the
        // download to the end.
        cmd.arg("-re");
        if at > 0.0 {
            // Seeking before `-i` is a byte-range seek: FFmpeg asks the stream for
            // that part of the file instead of decoding everything before it.
            cmd.arg("-ss").arg(format!("{at:.3}"));
        }
        cmd.arg("-i").arg(source);
        cmd.args(["-an", "-sn", "-dn"]);
        // Fixed output size with the letterbox baked in: the frame size is then a
        // constant, and nothing has to be parsed to know how to read stdout.
        cmd.arg("-vf").arg(format!(
            "scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease,\
             pad={WIDTH}:{HEIGHT}:(ow-iw)/2:(oh-ih)/2"
        ));
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| format!("starting FFmpeg: {e}"))?;
        let mut out = child
            .stdout
            .take()
            .ok_or_else(|| "FFmpeg has no output".to_string())?;

        let (tx, frames) = sync_channel::<Vec<u8>>(3);
        let stop = Arc::new(AtomicBool::new(false));

        // The length is printed once, before any frame; a separate reader keeps it
        // off the video path so a slow log line can never delay the picture.
        let (duration_tx, duration) = std::sync::mpsc::channel::<f64>();
        let tail: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        if let Some(err) = child.stderr.take() {
            let tail = tail.clone();
            let flag = stop.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if flag.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(seconds) = parse_duration(&line) {
                        let _ = duration_tx.send(seconds);
                    }
                    // The banner describes the input; only a complaint is worth
                    // keeping, and only until something is playing.
                    let trimmed = line.trim();
                    if is_complaint(trimmed) {
                        if let Ok(mut tail) = tail.lock() {
                            *tail = Some(trimmed.to_string());
                        }
                    }
                }
            });
        }

        let flag = stop.clone();
        std::thread::spawn(move || {
            let size = WIDTH * HEIGHT * 4;
            loop {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                let mut frame = vec![0u8; size];
                if out.read_exact(&mut frame).is_err() {
                    return; // the film ended, or the process was stopped
                }
                // If the interface is not reading — paused, or a window that is not
                // being redrawn — stop handing frames over. The pipe fills, FFmpeg
                // blocks on its next write, and decoding pauses with it.
                let mut pending = frame;
                loop {
                    if flag.load(Ordering::Relaxed) {
                        return;
                    }
                    match tx.try_send(pending) {
                        Ok(()) => break,
                        Err(TrySendError::Full(frame)) => {
                            pending = frame;
                            std::thread::sleep(Duration::from_millis(4));
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
            }
        });

        Ok(VideoPipe {
            child,
            frames,
            duration,
            tail,
            stop,
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for VideoPipe {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// The sound
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, target_os = "macos")))]
mod audio {
    use super::*;

    /// A platform without a sound backend here still gets the picture. The PCM is
    /// read and thrown away so FFmpeg is never blocked writing it.
    pub struct AudioProc {
        child: Child,
        stop: Arc<AtomicBool>,
    }

    impl AudioProc {
        pub fn spawn(
            ffmpeg: &std::path::Path,
            source: &str,
            at: f64,
            paused: Arc<AtomicBool>,
            // This platform has no sound backend here, so the level has nowhere to
            // go — but the signature is the same as the real one on purpose.
            _volume: Arc<AtomicU32>,
        ) -> Result<AudioProc, String> {
            let mut child = audio_command(ffmpeg, source, at)?
                .spawn()
                .map_err(|e| format!("starting FFmpeg for audio: {e}"))?;
            let mut out = child
                .stdout
                .take()
                .ok_or_else(|| "FFmpeg has no audio output".to_string())?;
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                loop {
                    if flag.load(Ordering::Relaxed) {
                        return;
                    }
                    // While paused, stop reading: FFmpeg then waits on the pipe.
                    if paused.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    match out.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                }
            });
            Ok(AudioProc { child, stop })
        }

        pub fn stop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod audio {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    type Ring = Arc<Mutex<VecDeque<f32>>>;

    pub struct AudioProc {
        child: Child,
        stop: Arc<AtomicBool>,
        /// Dropping the stream stops the device, so it is held for as long as the
        /// audio process lives — and no audio callback can outlive the player.
        _stream: cpal::Stream,
    }

    impl AudioProc {
        pub fn spawn(
            ffmpeg: &std::path::Path,
            source: &str,
            at: f64,
            paused: Arc<AtomicBool>,
            volume: Arc<AtomicU32>,
        ) -> Result<AudioProc, String> {
            let device = cpal::default_host()
                .default_output_device()
                .ok_or_else(|| "no audio output device".to_string())?;
            let supported = device
                .supported_output_configs()
                .map_err(|e| format!("audio device: {e}"))?
                .filter(|c| c.channels() == 2)
                .next()
                .ok_or_else(|| "the audio device has no stereo output".to_string())?;
            let sample_format = supported.sample_format();
            let config: cpal::StreamConfig = supported
                .try_with_sample_rate(cpal::SampleRate(48_000))
                .unwrap_or_else(|| supported.with_max_sample_rate())
                .config();

            let mut child = audio_command(ffmpeg, source, at)?
                .args([
                    "-ar",
                    &config.sample_rate.0.to_string(),
                    "-ac",
                    "2",
                    "-f",
                    "s16le",
                    "-",
                ])
                .spawn()
                .map_err(|e| format!("starting FFmpeg for audio: {e}"))?;
            let mut out = child
                .stdout
                .take()
                .ok_or_else(|| "FFmpeg has no audio output".to_string())?;

            let ring: Ring = Arc::new(Mutex::new(VecDeque::with_capacity(AUDIO_BUFFER)));
            let stop = Arc::new(AtomicBool::new(false));

            {
                let ring = ring.clone();
                let flag = stop.clone();
                let paused = paused.clone();
                std::thread::spawn(move || {
                    let mut raw = vec![0u8; 8192];
                    loop {
                        if flag.load(Ordering::Relaxed) {
                            return;
                        }
                        // Let the device drain before reading more: keeps the delay
                        // between pausing and silence down to a fraction of a second.
                        if paused.load(Ordering::Relaxed)
                            || ring.lock().map(|r| r.len()).unwrap_or(0) >= AUDIO_BUFFER
                        {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        let n = match out.read(&mut raw) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        if let Ok(mut ring) = ring.lock() {
                            for pair in raw[..n].chunks_exact(2) {
                                let sample = i16::from_le_bytes([pair[0], pair[1]]);
                                ring.push_back(sample as f32 / 32768.0);
                            }
                        }
                    }
                });
            }

            // A device error mid-playback is not fatal to the picture; the next
            // frame still arrives.
            let err_fn = |_e| {};
            let stream = match sample_format {
                cpal::SampleFormat::F32 => device.build_output_stream(
                    &config,
                    {
                        let ring = ring.clone();
                        let paused = paused.clone();
                        let volume = volume.clone();
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            fill(&ring, &paused, &volume, data)
                        }
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I16 => device.build_output_stream(
                    &config,
                    {
                        let ring = ring.clone();
                        let paused = paused.clone();
                        let volume = volume.clone();
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            fill(&ring, &paused, &volume, data)
                        }
                    },
                    err_fn,
                    None,
                ),
                other => return Err(format!("unsupported sample format {other:?}")),
            }
            .map_err(|e| format!("opening the audio device: {e}"))?;

            stream
                .play()
                .map_err(|e| format!("starting playback: {e}"))?;

            Ok(AudioProc {
                child,
                stop,
                _stream: stream,
            })
        }

        pub fn stop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Hand the device the next samples. While paused the buffer is left alone and
    /// silence is written, so playback resumes where it stopped.
    fn fill<T>(ring: &Ring, paused: &AtomicBool, volume: &AtomicU32, data: &mut [T])
    where
        T: cpal::Sample + cpal::FromSample<f32>,
    {
        // The gain is read once per buffer: a slider that moves mid-buffer would
        // otherwise make the level wobble sample by sample.
        let gain = volume.load(Ordering::Relaxed).min(100) as f32 / 100.0;
        if paused.load(Ordering::Relaxed) || gain == 0.0 {
            for slot in data.iter_mut() {
                *slot = T::from_sample(0.0f32);
            }
            return;
        }
        match ring.lock() {
            Ok(mut ring) => {
                for slot in data.iter_mut() {
                    *slot = T::from_sample(ring.pop_front().unwrap_or(0.0) * gain);
                }
            }
            Err(_) => {
                for slot in data.iter_mut() {
                    *slot = T::from_sample(0.0f32);
                }
            }
        }
    }
}

/// The command shared by both audio implementations: PCM on stdout, no video.
fn audio_command(ffmpeg: &std::path::Path, source: &str, at: f64) -> Result<Command, String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-nostdin", "-loglevel", "error", "-re"]);
    if at > 0.0 {
        cmd.arg("-ss").arg(format!("{at:.3}"));
    }
    cmd.arg("-i").arg(source);
    cmd.args(["-vn", "-sn", "-dn"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An eight-second test film — long enough that a seek lands inside it — or
    /// `None` when FFmpeg is not installed (the player is then merely unavailable,
    /// and the rest of the interface is unaffected).
    fn sample(dir: &std::path::Path) -> Option<PathBuf> {
        let ffmpeg = ffmpeg().ok()?;
        let path = dir.join("sample.mp4");
        let status = Command::new(ffmpeg)
            .args(["-nostdin", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc=size=320x240:rate=10"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", "8", "-pix_fmt", "yuv420p", "-shortest"])
            .arg(&path)
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    #[test]
    fn a_film_is_decoded_into_frames_of_the_advertised_size() {
        let dir = std::env::temp_dir().join("odm-player-decode");
        std::fs::create_dir_all(&dir).unwrap();
        let Some(path) = sample(&dir) else {
            eprintln!("ffmpeg is not installed; skipping the decode test");
            return;
        };

        let mut player = Player::open("sample", &path.to_string_lossy()).unwrap();
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && player.texture.is_none() {
            player.pump(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }

        let texture = player
            .texture
            .as_ref()
            .expect("no frame arrived from FFmpeg");
        assert_eq!(texture.size(), [WIDTH, HEIGHT]);
        assert!(player.position() > 0.0, "the clock never advanced");
    }

    #[test]
    fn seeking_restarts_the_decoder_at_the_new_position() {
        let dir = std::env::temp_dir().join("odm-player-seek");
        std::fs::create_dir_all(&dir).unwrap();
        let Some(path) = sample(&dir) else {
            eprintln!("ffmpeg is not installed; skipping the seek test");
            return;
        };

        let mut player = Player::open("sample", &path.to_string_lossy()).unwrap();
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && player.texture.is_none() {
            player.pump(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }

        player.skip(1.0);
        assert!(player.position() >= 1.0, "{}", player.position());

        // The restarted decoder still produces frames, which is the point of seeking:
        // it must not leave the player stuck on the frame it was showing.
        player.texture = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && player.texture.is_none() {
            player.pump(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            player.texture.is_some(),
            "no frame after seeking (error={:?}, ended={}, video={})",
            player.error,
            player.ended,
            player.video.is_some()
        );
    }

    #[test]
    fn the_length_is_read_from_ffmpegs_banner() {
        assert_eq!(
            parse_duration("  Duration: 00:01:23.45, start: 0.000000, bitrate: 1234 kb/s"),
            Some(83.45)
        );
        assert_eq!(
            parse_duration("  Duration: 01:02:03.00, start: 0.000000"),
            Some(3723.0)
        );
        // A live or malformed input has no length to show.
        assert_eq!(parse_duration("  Duration: N/A, start: 0.000000"), None);
        assert_eq!(parse_duration("  Stream #0:0: Video: h264"), None);
    }

    /// A player with no decoder — the state the control bar is drawn in while a
    /// stream is still buffering, and the state after a film has ended.
    fn idle_player() -> Player {
        Player {
            title: "a film".into(),
            source: "unused".into(),
            base: 0.0,
            anchor: Some(Instant::now()),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(100)),
            last_volume: 100,
            video: None,
            audio: None,
            texture: None,
            duration: Some(3723.0),
            scrub_to: None,
            ended: false,
            error: None,
        }
    }

    #[test]
    fn the_control_bar_draws_without_a_window() {
        // The player lives in a child viewport, which needs a real desktop to open;
        // the layout, the clock and the transport buttons do not. Drawing them here
        // catches a broken clock format or a panicking layout before a window is.
        let mut player = idle_player();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1024.0, 648.0),
            )),
            ..Default::default()
        };
        let mut draws = 0;
        for screen in [egui::vec2(1024.0, 648.0), egui::vec2(520.0, 340.0)] {
            let mut input = input.clone();
            input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen));
            let mut close = true;
            let mut out = ctx.run_ui(input, |ui| close = player.ui(ui));
            out.textures_delta.clear();
            assert!(!close, "the player asked to close itself at {screen:?}");
            draws += 1;
        }
        assert_eq!(draws, 2);
    }

    #[test]
    fn the_clock_reads_like_a_players() {
        assert_eq!(fmt_clock(0.0), "00:00");
        assert_eq!(fmt_clock(83.9), "01:23");
        assert_eq!(fmt_clock(3723.0), "1:02:03");
        // A position the clock is asked for before the first frame can be past the
        // end; showing "-1:-1" would be worse than showing the start.
        assert_eq!(fmt_clock(-4.0), "00:00");
    }

    #[test]
    fn pausing_freezes_the_clock_and_resuming_continues_it() {
        let mut player = idle_player();
        player.base = 12.0;
        std::thread::sleep(Duration::from_millis(60));
        let before = player.position();
        player.toggle_pause();
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            (player.position() - before).abs() < 0.001,
            "the clock ran while paused"
        );
        player.toggle_pause();
        std::thread::sleep(Duration::from_millis(60));
        assert!(player.position() > before);
    }
}
