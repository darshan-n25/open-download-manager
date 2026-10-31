// Open Download Manager — the native desktop app (egui/eframe).
//
// Standalone: starts the odm-cli daemon in-process on 127.0.0.1:1001, so the user
// just runs the exe (like IDM) without launching a separate daemon. The UI talks
// to the daemon's HTTP API.
//
// Layout follows the brief: a menu bar, a toolbar of icon actions, a category
// tree down the left, and a sortable jobs table in the middle — but styled dark
// and flat rather than the 2000s-era grey.

use eframe::egui;
use egui_lucide::Lucide;
use odm::config::Config;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

mod player;

/// The base URL of the in-process daemon, filled in once it has bound.
static DAEMON_ADDR: OnceLock<String> = OnceLock::new();

/// The runtime the streaming servers live on.
///
/// A broadcast is served by its own task, and it has to keep running while the
/// download that feeds it continues on the worker thread's runtime. The player's
/// stream server therefore gets a runtime of the interface, not of the download.
static STREAM_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn stream_rt() -> Option<&'static tokio::runtime::Runtime> {
    if let Some(rt) = STREAM_RT.get() {
        return Some(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("odm-stream")
        .build()
        .ok()?;
    let _ = STREAM_RT.set(rt);
    STREAM_RT.get()
}

// ---------------------------------------------------------------------------
// Data from the daemon
// ---------------------------------------------------------------------------

#[derive(Clone, serde::Deserialize, Default)]
struct Job {
    id: String,
    url: String,
    output: String,
    kind: String,
    state: String,
    downloaded: u64,
    total: Option<u64>,
    #[serde(default)]
    speed_bps: f64,
    connections: usize,
    quality: Option<String>,
    resumable: bool,
    #[serde(default)]
    checksum: Option<Checksum>,
    error: Option<String>,
    updated_ms: u64,
    quota_paused: bool,
}

#[derive(Clone, serde::Deserialize, Default)]
struct Checksum {
    algorithm: String,
    digest: String,
}

#[derive(Clone, serde::Deserialize, Default)]
struct Quota {
    limit: Option<u64>,
    used: u64,
    resets_in_secs: Option<u64>,
}

#[derive(Clone, serde::Deserialize, Default)]
struct Project {
    name: String,
    kind: String,
    archive: bool,
    enabled: bool,
    output: String,
    next_run_in: String,
    last_run: String,
    last_found: Option<u64>,
    last_error: Option<String>,
}

/// Where the in-process daemon is listening.
///
/// It is not always 1001: Windows reserves some ports (1001 was taken by the system on
/// the machine this was built on), so the app falls back to an ephemeral port and then
/// talks to whatever it actually got.
fn daemon_base() -> String {
    DAEMON_ADDR
        .get()
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:1001".to_string())
}

fn http_get<T: serde::de::DeserializeOwned>(path: &str) -> Option<T> {
    let url = format!("{}{}", daemon_base(), path);
    let resp = reqwest::blocking::get(&url).ok()?;
    resp.json().ok()
}

fn http_post(path: &str, body: &str) -> Option<String> {
    let url = format!("{}{}", daemon_base(), path);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .ok()?;
    Some(resp.text().unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Categories — the tree down the left
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Category {
    #[default]
    All,
    Unfinished,
    Finished,
    Failed,
    Documents,
    Compressed,
    Audio,
    Video,
    Programs,
    Projects,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::All => "All Downloads",
            Category::Unfinished => "Unfinished",
            Category::Finished => "Finished",
            Category::Failed => "Failed",
            Category::Documents => "Documents",
            Category::Compressed => "Compressed",
            Category::Audio => "Music",
            Category::Video => "Video",
            Category::Programs => "Programs",
            Category::Projects => "Grabber Projects",
        }
    }

    fn icon(self) -> Lucide {
        match self {
            Category::All => Lucide::FolderOpen,
            Category::Unfinished => Lucide::CirclePause,
            Category::Finished => Lucide::CircleCheck,
            Category::Failed => Lucide::CircleX,
            Category::Documents => Lucide::FileText,
            Category::Compressed => Lucide::FileArchive,
            Category::Audio => Lucide::Music,
            Category::Video => Lucide::Film,
            Category::Programs => Lucide::Package,
            Category::Projects => Lucide::Globe,
        }
    }

    /// Whether a job belongs in this category.
    fn matches(self, j: &Job) -> bool {
        match self {
            Category::All => true,
            Category::Unfinished => !j.state.eq_ignore_ascii_case("done"),
            Category::Finished => j.state.eq_ignore_ascii_case("done"),
            Category::Failed => j.state.eq_ignore_ascii_case("failed"),
            Category::Documents => is_doc(j),
            Category::Compressed => is_archive(j),
            Category::Audio => is_audio(j),
            Category::Video => is_video(j),
            Category::Programs => is_program(j),
            Category::Projects => false,
        }
    }
}

fn ext_of(j: &Job) -> String {
    let name = j.output.rsplit(['/', '\\']).next().unwrap_or("");
    match name.rfind('.') {
        Some(i) => name[i + 1..].to_ascii_lowercase(),
        None => String::new(),
    }
}

fn is_doc(j: &Job) -> bool {
    let e = ext_of(j);
    [
        "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "rtf", "odt", "epub",
    ]
    .contains(&e.as_str())
}

fn is_archive(j: &Job) -> bool {
    let e = ext_of(j);
    [
        "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "dmg", "cab",
    ]
    .contains(&e.as_str())
}

fn is_audio(j: &Job) -> bool {
    let e = ext_of(j);
    [
        "mp3", "flac", "wav", "aac", "ogg", "m4a", "wma", "opus", "aiff",
    ]
    .contains(&e.as_str())
}

fn is_video(j: &Job) -> bool {
    let e = ext_of(j);
    [
        "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "m3u8",
    ]
    .contains(&e.as_str())
}

fn is_program(j: &Job) -> bool {
    let e = ext_of(j);
    ["exe", "msi", "apk", "deb", "rpm", "pkg", "bin", "appimage"].contains(&e.as_str())
}

// ---------------------------------------------------------------------------
// Table sorting
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Size,
    Status,
    Eta,
    Rate,
    Updated,
}

#[derive(Clone, Copy)]
struct SortState {
    by: SortBy,
    asc: bool,
}

impl Default for SortState {
    fn default() -> Self {
        SortState {
            by: SortBy::Name,
            asc: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Tab {
    #[default]
    Downloads,
    Projects,
    Torrents,
}

/// One torrent download, shared between the UI and the worker thread that runs it.
#[derive(Clone)]
struct TorrentJob {
    id: u64,
    name: String,
    info_hash: String,
    /// Where the torrent's files live: the download directory.
    base: PathBuf,
    /// The biggest file, which is what `output_path` reports.
    output: PathBuf,
    total: u64,
    downloaded: u64,
    speed_bps: f64,
    peers: usize,
    /// "connecting", "downloading", "done", "failed" or "cancelled".
    state: String,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
    /// Set while the user has paused this torrent. Kept on the job rather than only
    /// on the session so a pause requested before the metadata arrives still holds
    /// when the session is created.
    pause: Arc<AtomicBool>,
    /// What the job was added from, so a failed or cancelled torrent can be retried
    /// without asking the user for the file or the magnet link again.
    source: Option<TorrentSource>,
    /// The live session, once the metadata is known. Streaming goes through it, and
    /// it is dropped (stopping the download's tasks) when the UI forgets the job.
    session: Option<Arc<odm::bt::session::Session>>,
    /// The files in the torrent, as (name, size) — what the player can choose from.
    files: Vec<(String, u64)>,
}

// ---------------------------------------------------------------------------
// The app
// ---------------------------------------------------------------------------

#[derive(Default)]
struct App {
    jobs: Vec<Job>,
    quota: Quota,
    projects: Vec<Project>,
    url_input: String,
    status: String,
    last_refresh: Option<std::time::Instant>,
    daemon_ok: bool,
    category: Category,
    /// The row whose details are shown. Clicking a row focuses it; the checkbox is
    /// a separate gesture, so a batch does not replace what the details strip says.
    focused: Option<String>,
    /// Downloads ticked for a batch action.
    checked: BTreeSet<String>,
    /// Torrents ticked for a batch action.
    checked_torrents: BTreeSet<u64>,
    /// The torrent the player is showing, so deleting it closes the player too.
    playing_torrent: Option<u64>,
    sort: SortState,
    tab: Tab,
    show_add: bool,
    /// `None` means "whatever the daemon is configured to use". A slider would silently
    /// clamp a zero into one, and a download with one connection is a fifth of the
    /// speed — so the number is only sent when the user actually asks for one.
    add_connections: Option<usize>,
    add_output: String,
    add_quality: String,
    /// The path of a torrent to add, for the times a file cannot be dragged.
    add_torrent_path: String,
    search: String,
    /// The daemon's configured number of connections, shown in the Add URL dialog.
    default_connections: usize,
    /// Where the interface's controls were drawn, by name.
    ///
    /// egui does not expose a list of widgets, so this is how a test finds the button
    /// it wants to click: it reads the rectangle and clicks the middle of it.
    rects: BTreeMap<String, egui::Rect>,
    /// The torrent downloads, shared with the worker threads that run them.
    torrents: Arc<std::sync::Mutex<Vec<TorrentJob>>>,
    /// The player, when something is playing.
    player: Option<player::Player>,
    /// A stream server that is being started: the UI polls for its URL each frame.
    starting_play: Option<(std::sync::mpsc::Receiver<Result<String, String>>, String)>,
    /// The next torrent's id, so a job can be found even after a magnet discovers
    /// its real name.
    next_torrent_id: u64,
    /// Stops the stream server that is feeding the player. Closing the player must
    /// release its port; without this every play would leave a server behind.
    stream_stop: Option<Arc<AtomicBool>>,
}

impl App {
    /// The window starts with the daemon's own settings in view, so the Add URL dialog
    /// offers the connection count the machine is actually configured for.
    fn new() -> Self {
        let cfg = Config::load();
        App {
            default_connections: cfg.connections.max(1),
            ..Default::default()
        }
    }

    fn refresh(&mut self) {
        if let Some(jobs) = http_get::<Vec<Job>>("/api/jobs") {
            self.jobs = jobs;
            self.daemon_ok = true;
        } else {
            self.daemon_ok = false;
        }
        if let Some(q) = http_get::<Quota>("/api/quota") {
            self.quota = q;
        }
        if let Some(p) = http_get::<Vec<Project>>("/api/projects") {
            self.projects = p;
        }
        self.prune_checked();
        self.last_refresh = Some(std::time::Instant::now());
    }

    fn visible_jobs(&self) -> Vec<Job> {
        let mut v: Vec<Job> = self
            .jobs
            .iter()
            .filter(|j| self.category.matches(j))
            .filter(|j| {
                if self.search.is_empty() {
                    return true;
                }
                let s = self.search.to_ascii_lowercase();
                j.output.to_ascii_lowercase().contains(&s)
                    || j.url.to_ascii_lowercase().contains(&s)
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            let ord = match self.sort.by {
                SortBy::Name => a
                    .output
                    .to_ascii_lowercase()
                    .cmp(&b.output.to_ascii_lowercase()),
                SortBy::Size => a.total.unwrap_or(0).cmp(&b.total.unwrap_or(0)),
                SortBy::Status => a.state.cmp(&b.state),
                SortBy::Eta => eta_secs(a).cmp(&eta_secs(b)),
                SortBy::Rate => a
                    .speed_bps
                    .partial_cmp(&b.speed_bps)
                    .unwrap_or(std::cmp::Ordering::Equal),
                SortBy::Updated => a.updated_ms.cmp(&b.updated_ms),
            };
            if self.sort.asc {
                ord
            } else {
                ord.reverse()
            }
        });
        v
    }

    fn selected_job(&self) -> Option<&Job> {
        let id = self.focused.as_ref()?;
        self.jobs.iter().find(|j| &j.id == id)
    }

    fn act(&mut self, id: &str, action: &str) {
        if let Some(t) = http_post(&format!("/api/jobs/{id}/{action}"), "{}") {
            self.status = t;
        }
        self.refresh();
    }

    fn act_all(&mut self, action: &str) {
        // On the Torrents tab the toolbar means the torrents: the two lists are
        // separate, so "Stop" there must not quietly pause downloads nobody is
        // looking at.
        if self.tab == Tab::Torrents {
            let ids: Vec<u64> = self
                .torrents
                .lock()
                .unwrap()
                .iter()
                .filter(|j| !matches!(j.state.as_str(), "done" | "failed" | "cancelled"))
                .map(|j| j.id)
                .collect();
            self.torrents_action(&ids, action);
            return;
        }
        let ids: Vec<String> = self
            .jobs
            .iter()
            .filter(|j| !j.state.eq_ignore_ascii_case("done"))
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            let _ = http_post(&format!("/api/jobs/{id}/{action}"), "{}");
        }
        self.refresh();
    }

    /// Run a daemon action on an explicit set of downloads — the batch path.
    fn act_ids(&mut self, ids: &[String], action: &str) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            let _ = http_post(&format!("/api/jobs/{id}/{action}"), "{}");
        }
        self.status = format!(
            "{} {} download(s)",
            match action {
                "resume" => "started",
                "pause" => "paused",
                "remove" => "removed",
                other => other,
            },
            ids.len()
        );
        self.refresh();
    }

    fn act_checked(&mut self, action: &str) {
        let ids: Vec<String> = self.checked.iter().cloned().collect();
        self.act_ids(&ids, action);
        if action == "remove" {
            self.checked.clear();
            if self
                .focused
                .as_ref()
                .is_some_and(|id| !self.jobs.iter().any(|j| &j.id == id))
            {
                self.focused = None;
            }
        }
    }

    /// The toolbar's Delete: whatever is ticked, or the focused row when nothing is.
    fn remove_focused_or_checked(&mut self) {
        if self.tab == Tab::Torrents {
            let ids: Vec<u64> = self.checked_torrents.iter().copied().collect();
            if !ids.is_empty() {
                self.remove_torrents(&ids);
            }
            return;
        }
        if !self.checked.is_empty() {
            self.act_checked("remove");
        } else if let Some(id) = self.focused.clone() {
            self.act(&id, "remove");
            self.focused = None;
        }
    }

    /// Keep the tick boxes honest when a refresh drops jobs that have gone away.
    fn prune_checked(&mut self) {
        self.checked
            .retain(|id| self.jobs.iter().any(|j| &j.id == id));
        let ids: std::collections::HashSet<u64> =
            self.torrents.lock().unwrap().iter().map(|j| j.id).collect();
        self.checked_torrents.retain(|id| ids.contains(id));
    }

    fn delete_finished(&mut self) {
        let ids: Vec<String> = self
            .jobs
            .iter()
            .filter(|j| j.state.eq_ignore_ascii_case("done"))
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            let _ = http_post(&format!("/api/jobs/{id}/remove"), "{}");
        }
        self.refresh();
    }

    fn queue_start(&mut self) {
        if let Some(t) = http_post("/api/queue/start", "{}") {
            self.status = t;
        }
        self.refresh();
    }

    fn queue_stop(&mut self) {
        if let Some(t) = http_post("/api/queue/stop", "{}") {
            self.status = t;
        }
        self.refresh();
    }

    fn add_job(&mut self) {
        let url = self.url_input.trim().to_string();
        if url.is_empty() {
            return;
        }
        let mut body = serde_json::json!({ "url": url });
        if let Some(n) = self.add_connections {
            body["connections"] = serde_json::json!(n.to_string());
        }
        if !self.add_output.trim().is_empty() {
            body["output"] = serde_json::json!(self.add_output.trim());
        }
        if !self.add_quality.trim().is_empty() {
            body["quality"] = serde_json::json!(self.add_quality.trim());
        }
        match http_post("/api/jobs", &body.to_string()) {
            Some(t) => {
                self.status = t;
                self.show_add = false;
                self.url_input.clear();
                self.add_output.clear();
                self.add_quality.clear();
            }
            None => self.status = "Failed to queue".into(),
        }
        self.refresh();
    }
}

fn eta_secs(j: &Job) -> u64 {
    let Some(t) = j.total else { return 0 };
    eta_secs_bytes(t.saturating_sub(j.downloaded), j.speed_bps)
}

/// How long the remaining bytes take at the current rate. Zero means "nothing
/// honest to say": no speed, no total, or nothing left.
fn eta_secs_bytes(left: u64, speed_bps: f64) -> u64 {
    if speed_bps <= 0.0 || left == 0 {
        return 0;
    }
    (left as f64 / speed_bps) as u64
}

/// `1h 02m`, `5m 03s`, `42s` — the shape a download list wants.
fn fmt_secs(s: u64) -> String {
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

fn fmt_eta(j: &Job) -> String {
    if j.state.eq_ignore_ascii_case("done") {
        return String::new();
    }
    let s = eta_secs(j);
    if s == 0 {
        return "—".into();
    }
    fmt_secs(s)
}

/// How much of a torrent is finished, as a fraction for the progress bar.
///
/// A finished torrent is *exactly* complete: the engine counts verified bytes, so
/// the last piece can leave the bar at 99.9% of the total, and a bar that rounds
/// that down to "99%" over a file that is finished looks like a broken download.
fn torrent_fraction(job: &TorrentJob) -> f32 {
    if job.state == "done" || (job.total > 0 && job.downloaded >= job.total) {
        return 1.0;
    }
    if job.total == 0 {
        return 0.0;
    }
    (job.downloaded as f64 / job.total as f64).clamp(0.0, 1.0) as f32
}

fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut i = 0;
    let mut v = n as f64;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, U[i])
    } else {
        format!("{v:.2} {}", U[i])
    }
}

fn state_color(state: &str) -> egui::Color32 {
    match state {
        "done" => egui::Color32::from_rgb(74, 222, 128),
        "paused" => egui::Color32::from_rgb(251, 191, 36),
        "active" => egui::Color32::from_rgb(96, 165, 250),
        "failed" => egui::Color32::from_rgb(248, 113, 113),
        _ => egui::Color32::from_rgb(148, 163, 184),
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl App {
    /// Draw the whole interface.
    ///
    /// Separate from the `eframe::App` method so that a test can drive it without a
    /// window: the interface is then exercised the way a user exercises it — a click
    /// at a point — but in a process that needs no display and no mouse.
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let elapsed = self
            .last_refresh
            .map(|t| t.elapsed())
            .unwrap_or(Duration::from_secs(10));
        if elapsed > Duration::from_millis(1200) {
            self.refresh();
        }

        self.menu_bar(ui);
        self.toolbar(ui);
        self.tab_strip(ui);

        egui::Panel::left("categories")
            .exact_size(200.0)
            .resizable(false)
            .show(ui, |ui| self.categories(ui));

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        // A stream that was being started gets its URL here, once the server has
        // bound — the UI must never block waiting for a socket.
        self.poll_starting_play();

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Downloads => self.downloads(ui),
            Tab::Projects => self.projects_tab(ui),
            Tab::Torrents => self.torrents_tab(ui),
        });

        if self.show_add {
            self.add_dialog(&ctx);
        }

        // The player is a window of its own, not a pane at the bottom of the list:
        // a film is the reason the window exists, and a strip of it is not what
        // "stream" should mean.
        self.player_window(&ctx);

        // A film needs a fresh frame far more often than a download list does.
        if self.player.is_some() || self.starting_play.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(Duration::from_millis(1200));
        }
    }
}

impl App {
    // ----- menu bar -------------------------------------------------------
    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Tasks", |ui| {
                if ui.button("Add URL…").clicked() {
                    self.show_add = true;
                    ui.close();
                }
                if ui.button("Start Queue").clicked() {
                    self.queue_start();
                    ui.close();
                }
                if ui.button("Stop Queue").clicked() {
                    self.queue_stop();
                    ui.close();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("File", |ui| {
                if ui.button("Delete Completed").clicked() {
                    self.delete_finished();
                    ui.close();
                }
                if ui.button("Delete All").clicked() {
                    self.act_all("remove");
                    ui.close();
                }
            });
            ui.menu_button("Downloads", |ui| {
                if ui.button("Resume All").clicked() {
                    self.act_all("resume");
                    ui.close();
                }
                if ui.button("Pause All").clicked() {
                    self.act_all("pause");
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                let mut t = self.tab;
                if ui
                    .selectable_value(&mut t, Tab::Downloads, "Downloads")
                    .clicked()
                {
                    self.tab = t;
                    ui.close();
                }
                if ui
                    .selectable_value(&mut t, Tab::Projects, "Projects")
                    .clicked()
                {
                    self.tab = t;
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About Open Download Manager").clicked() {
                    self.status =
                        concat!(
                            "Open Download Manager ",
                            env!("CARGO_PKG_VERSION"),
                            " — an open-source download manager"
                        )
                        .into();
                    ui.close();
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // The connection state is a development aid: it says whether the
                // in-process daemon is up, which is the first question when the window
                // looks empty. It is not part of the finished interface, so it is
                // compiled out of a release build and only shown while developing.
                if cfg!(debug_assertions) {
                    if self.daemon_ok {
                        ui.label(
                            egui::RichText::new("● online")
                                .color(egui::Color32::from_rgb(74, 222, 128))
                                .size(12.0),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("● starting…")
                                .color(egui::Color32::from_rgb(251, 191, 36))
                                .size(12.0),
                        );
                    }
                }
            });
        });
    }

    // ----- toolbar --------------------------------------------------------
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(24, 26, 32))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;

                    self.tool_button(
                        ui,
                        Lucide::Plus,
                        "Add URL",
                        egui::Color32::from_rgb(96, 165, 250),
                        |s| {
                            s.show_add = true;
                        },
                    );
                    self.tool_button(
                        ui,
                        Lucide::Play,
                        "Resume",
                        egui::Color32::from_rgb(74, 222, 128),
                        |s| {
                            s.act_all("resume");
                        },
                    );
                    self.tool_button(
                        ui,
                        Lucide::Square,
                        "Stop",
                        egui::Color32::from_rgb(248, 113, 113),
                        |s| {
                            s.act_all("pause");
                        },
                    );
                    self.tool_button(
                        ui,
                        Lucide::Trash,
                        "Delete",
                        egui::Color32::from_rgb(148, 163, 184),
                        |s| {
                            s.remove_focused_or_checked();
                        },
                    );
                    self.tool_button(
                        ui,
                        Lucide::Trash2,
                        "Delete Completed",
                        egui::Color32::from_rgb(148, 163, 184),
                        |s| {
                            s.delete_finished();
                        },
                    );

                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);

                    self.tool_button(
                        ui,
                        Lucide::Settings,
                        "Options",
                        egui::Color32::from_rgb(167, 139, 250),
                        |s| {
                            s.status =
                                "Set limits with `odm-cli config set` (speed_limit, quota_bytes)"
                                    .into();
                        },
                    );
                    self.tool_button(
                        ui,
                        Lucide::Clock,
                        "Scheduler",
                        egui::Color32::from_rgb(251, 191, 36),
                        |s| {
                            s.tab = Tab::Projects;
                        },
                    );
                    self.tool_button(
                        ui,
                        Lucide::CirclePlay,
                        "Start Queue",
                        egui::Color32::from_rgb(96, 165, 250),
                        |s| {
                            s.queue_start();
                        },
                    );
                    self.tool_button(
                        ui,
                        Lucide::CircleStop,
                        "Stop Queue",
                        egui::Color32::from_rgb(248, 113, 113),
                        |s| {
                            s.queue_stop();
                        },
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("Search downloads…")
                                .desired_width(200.0),
                        );
                    });
                });
            });
    }

    fn tool_button<F: FnOnce(&mut Self)>(
        &mut self,
        ui: &mut egui::Ui,
        icon: Lucide,
        label: &'static str,
        color: egui::Color32,
        f: F,
    ) {
        // One square button, no caption: the icon is the whole control and the name
        // arrives as a tooltip when the pointer rests on it. The plate is invisible
        // until then, so the row reads as a set of symbols rather than a strip of
        // boxes — and it is a single egui button, so both the click and the hover
        // are the widget's own.
        let mut style = ui.style().as_ref().clone();
        let rest = egui::Color32::TRANSPARENT;
        let hover = egui::Color32::from_rgb(45, 50, 62);
        let press = egui::Color32::from_rgb(37, 99, 235);
        style.visuals.widgets.inactive.weak_bg_fill = rest;
        style.visuals.widgets.inactive.bg_fill = rest;
        style.visuals.widgets.hovered.weak_bg_fill = hover;
        style.visuals.widgets.hovered.bg_fill = hover;
        style.visuals.widgets.active.weak_bg_fill = press;
        style.visuals.widgets.active.bg_fill = press;
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(7);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(7);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(7);
        style.spacing.button_padding = egui::vec2(6.0, 6.0);

        let mut clicked = false;
        ui.scope(|ui| {
            ui.set_style(style);
            let response = ui
                .add(
                    egui::Button::new(icon.size(18.0).color(color).image())
                        .min_size(egui::vec2(32.0, 30.0)),
                )
                .on_hover_text(label);
            self.rects.insert(format!("tool.{label}"), response.rect);
            clicked = response.clicked();
        });
        if clicked {
            f(self);
        }
    }

    // ----- category tree --------------------------------------------------
    fn categories(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut category_row = |ui: &mut egui::Ui, cat: Category| {
                let selected = self.category == cat;
                // The whole row is one interactive area, allocated first and painted
                // behind the icon and the label. A side panel does not pass a
                // hand-made `ui.interact` the click, and an image or a label does not
                // react to one on its own — so the row is allocated with the sense,
                // and the children are drawn inside it.
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(184.0, 22.0), egui::Sense::click());
                if selected {
                    ui.painter()
                        .rect_filled(rect, 5.0, egui::Color32::from_rgb(37, 99, 235));
                } else if response.hovered() {
                    ui.painter()
                        .rect_filled(rect, 5.0, egui::Color32::from_rgb(45, 50, 60));
                }
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            cat.icon()
                                .size(15.0)
                                .color(if selected {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_rgb(148, 163, 184)
                                })
                                .image()
                                .sense(egui::Sense::empty()),
                        );
                        ui.add(egui::Label::new(
                            egui::RichText::new(cat.label())
                                .size(13.0)
                                .color(if selected {
                                    egui::Color32::WHITE
                                } else {
                                    ui.visuals().text_color()
                                }),
                        ));
                    });
                });
                if response.clicked() {
                    self.category = cat;
                }
            };

            category_row(ui, Category::All);
            ui.add_space(4.0);
            for cat in [Category::Unfinished, Category::Finished, Category::Failed] {
                category_row(ui, cat);
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("CATEGORIES")
                    .size(10.0)
                    .color(egui::Color32::GRAY),
            );
            ui.add_space(2.0);
            for cat in [
                Category::Documents,
                Category::Compressed,
                Category::Audio,
                Category::Video,
                Category::Programs,
            ] {
                category_row(ui, cat);
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("OTHER")
                    .size(10.0)
                    .color(egui::Color32::GRAY),
            );
            ui.add_space(2.0);
            category_row(ui, Category::Projects);
        });
    }

    // ----- downloads table -------------------------------------------------
    fn downloads(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let visible = self.visible_jobs();
                let total: u64 = visible.iter().filter_map(|j| j.total).sum();
                ui.label(
                    egui::RichText::new(format!("{} jobs", visible.len()))
                        .size(12.0)
                        .color(egui::Color32::GRAY),
                );
                if total > 0 {
                    ui.label(
                        egui::RichText::new(fmt_bytes(total))
                            .size(12.0)
                            .color(egui::Color32::GRAY),
                    );
                }
            });
        });
        ui.separator();

        // Batch bar, the way IDM's selection mode works: ticking any row brings up
        // the actions that make sense for a group, so ten downloads do not need ten
        // clicks each.
        if !self.checked.is_empty() {
            let mut action: Option<&str> = None;
            let mut clear = false;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{} selected", self.checked.len()))
                        .size(12.5)
                        .strong(),
                );
                if ui.button("▶ Start").clicked() {
                    action = Some("resume");
                }
                if ui.button("⏸ Pause").clicked() {
                    action = Some("pause");
                }
                if ui.button("🗑 Delete").clicked() {
                    action = Some("remove");
                }
                if ui.button("Clear").clicked() {
                    clear = true;
                }
            });
            if let Some(a) = action {
                self.act_checked(a);
            }
            if clear {
                self.checked.clear();
            }
        }

        if !self.daemon_ok {
            ui.centered_and_justified(|ui| {
                ui.add_space(40.0);
                ui.label(egui::RichText::new("Starting the daemon…").size(16.0));
            });
            return;
        }

        let jobs = self.visible_jobs();
        if jobs.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.add_space(60.0);
                ui.label(
                    egui::RichText::new("No downloads here yet")
                        .size(16.0)
                        .color(egui::Color32::GRAY),
                );
                ui.label(
                    egui::RichText::new("Use Add URL, or let the browser extension hand one over.")
                        .size(13.0)
                        .color(egui::Color32::GRAY),
                );
            });
            return;
        }

        let mut sort_clicked: Option<SortBy> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(30, 32, 40))
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut header =
                            |ui: &mut egui::Ui, label: &str, by: SortBy, width: f32| {
                                let mut r = egui::RichText::new(label)
                                    .size(11.5)
                                    .strong()
                                    .color(egui::Color32::from_rgb(156, 163, 175));
                                if self.sort.by == by {
                                    r = r.color(egui::Color32::from_rgb(96, 165, 250));
                                }
                                let row = ui.horizontal(|ui| {
                                    ui.add_sized([width, 20.0], egui::Label::new(r));
                                    if self.sort.by == by {
                                        ui.label(if self.sort.asc { "▲" } else { "▼" });
                                    }
                                });
                                // A label is not interactive, so the header claims the
                                // click for itself.
                                let resp = ui.interact(
                                    row.response.rect,
                                    ui.id().with(("sort", label)),
                                    egui::Sense::click(),
                                );
                                if resp.clicked() {
                                    sort_clicked = Some(by);
                                }
                            };
                        let mut all =
                            !jobs.is_empty() && jobs.iter().all(|j| self.checked.contains(&j.id));
                        if ui
                            .add(egui::Checkbox::without_text(&mut all))
                            .on_hover_text("Select all")
                            .clicked()
                        {
                            for j in &jobs {
                                if all {
                                    self.checked.insert(j.id.clone());
                                } else {
                                    self.checked.remove(&j.id);
                                }
                            }
                        }
                        header(ui, "File Name", SortBy::Name, 212.0);
                        header(ui, "Size", SortBy::Size, 82.0);
                        header(ui, "Status", SortBy::Status, 74.0);
                        header(ui, "Time left", SortBy::Eta, 74.0);
                        header(ui, "Transfer rate", SortBy::Rate, 96.0);
                        header(ui, "Last Try", SortBy::Updated, 86.0);
                        // The description is not a sort key: it is whatever the engine
                        // knows about the job, so it gets no arrow.
                        ui.add_sized(
                            [180.0, 20.0],
                            egui::Label::new(
                                egui::RichText::new("Description")
                                    .size(11.5)
                                    .strong()
                                    .color(egui::Color32::from_rgb(156, 163, 175)),
                            ),
                        );
                    });
                });

            for job in jobs {
                let focused = self.focused.as_deref() == Some(job.id.as_str());
                let checked = self.checked.contains(&job.id);
                let bg = if focused {
                    egui::Color32::from_rgb(37, 99, 235)
                } else if checked {
                    egui::Color32::from_rgb(31, 41, 59)
                } else {
                    egui::Color32::from_rgb(24, 26, 32)
                };
                let mut tick_rect = None;
                let row = egui::Frame::new()
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(8, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let mut ticked = checked;
                            let tick = ui.add(egui::Checkbox::without_text(&mut ticked));
                            tick_rect = Some(tick.rect);
                            self.rects
                                .insert(format!("job.check.{}", job.id), tick.rect);
                            if tick.changed() {
                                if ticked {
                                    self.checked.insert(job.id.clone());
                                } else {
                                    self.checked.remove(&job.id);
                                }
                            }
                            ui.horizontal(|ui| {
                                ui.add(job_icon(&job).size(16.0).color(state_color(&job.state)));
                                ui.vertical(|ui| {
                                    ui.add_sized(
                                        [208.0, 18.0],
                                        egui::Label::new(
                                            egui::RichText::new(
                                                job.output
                                                    .rsplit(['/', '\\'])
                                                    .next()
                                                    .unwrap_or(&job.output),
                                            )
                                            .size(13.0),
                                        ),
                                    );
                                    // The bar under the name is the at-a-glance progress,
                                    // the way IDM shows it: a column of percentages is
                                    // harder to read at a glance than a line of colour.
                                    if job.state == "active" {
                                        let pct = job
                                            .total
                                            .map(|t| job.downloaded as f32 / t as f32)
                                            .unwrap_or(0.0);
                                        ui.add(
                                            egui::ProgressBar::new(pct)
                                                .desired_width(204.0)
                                                .fill(egui::Color32::from_rgb(96, 165, 250)),
                                        );
                                    }
                                });
                            });
                            ui.add_sized(
                                [82.0, 20.0],
                                egui::Label::new(
                                    egui::RichText::new(match job.total {
                                        Some(t) => fmt_bytes(t),
                                        None => "—".into(),
                                    })
                                    .size(12.5)
                                    .color(egui::Color32::from_rgb(200, 205, 215)),
                                ),
                            );
                            ui.add_sized(
                                [82.0, 20.0],
                                egui::Label::new(
                                    egui::RichText::new(job.state.clone())
                                        .size(12.5)
                                        .color(state_color(&job.state)),
                                ),
                            );
                            ui.add_sized(
                                [74.0, 20.0],
                                egui::Label::new(
                                    egui::RichText::new(fmt_eta(&job))
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(200, 205, 215)),
                                ),
                            );
                            ui.add_sized(
                                [96.0, 20.0],
                                egui::Label::new(
                                    egui::RichText::new(if job.speed_bps > 0.0 {
                                        format!("{}/s", fmt_bytes(job.speed_bps as u64))
                                    } else {
                                        "—".into()
                                    })
                                    .size(12.5)
                                    .color(egui::Color32::from_rgb(200, 205, 215)),
                                ),
                            );
                            ui.add_sized(
                                [86.0, 20.0],
                                egui::Label::new(
                                    egui::RichText::new(format_time(job.updated_ms))
                                        .size(12.0)
                                        .color(egui::Color32::GRAY),
                                ),
                            );
                            ui.add_sized(
                                [180.0, 20.0],
                                egui::Label::new(
                                    egui::RichText::new(job_description(&job))
                                        .size(11.5)
                                        .color(egui::Color32::GRAY),
                                ),
                            );
                        });
                    });
                // The row is a frame of labels, so the click is claimed for the row
                // itself — otherwise the job list cannot be selected at all. The
                // checkbox is carved out of that rect: an interact registered after
                // the checkbox sits on top of it and would swallow every tick, which
                // is why single selection used to do nothing while "select all" (no
                // row interact over it) worked.
                let mut hit = row.response.rect;
                if let Some(tick) = tick_rect {
                    hit.min.x = (tick.max.x + ui.spacing().item_spacing.x).min(hit.max.x);
                }
                let resp = ui.interact(
                    hit,
                    ui.id().with(("job", job.id.clone())),
                    egui::Sense::click(),
                );
                if resp.hovered() && !focused {
                    // A bar down the left edge, rather than a fill: a fill painted over
                    // the row would hide the text it is meant to draw attention to.
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(row.response.rect.left(), row.response.rect.top()),
                            egui::vec2(2.0, row.response.rect.height()),
                        ),
                        1.0,
                        egui::Color32::from_rgb(96, 165, 250),
                    );
                }
                if resp.clicked() {
                    self.focused = Some(job.id.clone());
                }
            }
        });

        if let Some(by) = sort_clicked {
            if self.sort.by == by {
                self.sort.asc = !self.sort.asc;
            } else {
                self.sort.by = by;
                self.sort.asc = true;
            }
        }

        // Detail strip for the selected job: the features that have no column.
        if let Some(job) = self.selected_job().cloned() {
            ui.separator();
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(24, 26, 32))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Details").strong());
                        ui.add_space(8.0);
                        let pct = job
                            .total
                            .map(|t| job.downloaded as f32 / t as f32)
                            .unwrap_or(0.0);
                        ui.add(
                            egui::ProgressBar::new(pct)
                                .desired_width(220.0)
                                .show_percentage(),
                        );
                        ui.add_space(8.0);
                        ui.label(format!(
                            "{} / {}",
                            fmt_bytes(job.downloaded),
                            job.total.map(fmt_bytes).unwrap_or_else(|| "?".into())
                        ));
                        ui.add_space(16.0);
                        ui.label(format!("{} connection(s)", job.connections));
                        if job.resumable {
                            ui.label("resumable");
                        }
                        if let Some(q) = &job.quality {
                            ui.label(format!("quality: {q}"));
                        }
                        if let Some(c) = &job.checksum {
                            ui.label(format!(
                                "{}: {}",
                                c.algorithm,
                                &c.digest[..c.digest.len().min(12)]
                            ));
                        }
                        if job.quota_paused {
                            ui.label(
                                egui::RichText::new("paused on quota")
                                    .color(egui::Color32::from_rgb(251, 191, 36)),
                            );
                        }
                        if let Some(e) = &job.error {
                            ui.label(
                                egui::RichText::new(e)
                                    .color(egui::Color32::from_rgb(248, 113, 113)),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Show path").clicked() {
                                self.status = format!("Output: {}", job.output);
                            }
                        });
                    });
                });
        }
    }

    // ----- projects -------------------------------------------------------
    fn projects_tab(&mut self, ui: &mut egui::Ui) {
        if self.projects.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.add_space(50.0);
                ui.label(
                    egui::RichText::new("No projects")
                        .size(16.0)
                        .color(egui::Color32::GRAY),
                );
                ui.label(
                    egui::RichText::new(
                        "Create one with `odm-cli project add` — a file kept in sync, a site \
                         re-crawled on a schedule, or an offline mirror.",
                    )
                    .size(13.0)
                    .color(egui::Color32::GRAY),
                );
            });
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for p in self.projects.clone() {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(24, 26, 32))
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(if p.archive {
                                Lucide::Archive.size(18.0)
                            } else {
                                Lucide::Globe.size(18.0)
                            });
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&p.name).strong().size(14.0));
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} · {} · next run {} · last {}{}",
                                        p.kind,
                                        p.output,
                                        p.next_run_in,
                                        p.last_run,
                                        match p.last_found {
                                            Some(n) => format!(" · {n} file(s)"),
                                            None => String::new(),
                                        }
                                    ))
                                    .size(12.0)
                                    .color(egui::Color32::GRAY),
                                );
                                if !p.enabled {
                                    ui.label(
                                        egui::RichText::new("disabled")
                                            .size(11.5)
                                            .color(egui::Color32::from_rgb(148, 163, 184)),
                                    );
                                }
                                if let Some(e) = &p.last_error {
                                    ui.label(
                                        egui::RichText::new(e)
                                            .size(12.0)
                                            .color(egui::Color32::from_rgb(248, 113, 113)),
                                    );
                                }
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Run now").clicked() {
                                        let _ = http_post(
                                            &format!("/api/projects/{}/run", p.name),
                                            "{}",
                                        );
                                        self.refresh();
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(6.0);
            }
        });
    }

    // ----- the tabs -------------------------------------------------------
    ///
    /// Drawn once, in the frame, rather than inside each tab: a strip that belongs to
    /// one tab is a strip that disappears when you go to another one, and there is
    /// then no way back.
    fn tab_strip(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let r = ui.selectable_value(&mut self.tab, Tab::Downloads, "Downloads");
            self.rects.insert("tab.downloads".into(), r.rect);
            let r = ui.selectable_value(&mut self.tab, Tab::Projects, "Projects");
            self.rects.insert("tab.projects".into(), r.rect);
            let r = ui.selectable_value(&mut self.tab, Tab::Torrents, "Torrents");
            self.rects.insert("tab.torrents".into(), r.rect);
        });
        ui.separator();
    }

    // ----- torrents -------------------------------------------------------
    fn torrents_tab(&mut self, ui: &mut egui::Ui) {
        // A dropped file is the natural way to hand a torrent over.
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
            {
                self.add_torrent(&path);
            }
        }

        ui.horizontal(|ui| {
            ui.label("Torrent file:");
            ui.add_sized(
                [320.0, 24.0],
                egui::TextEdit::singleline(&mut self.add_torrent_path)
                    .hint_text("C:\\Downloads\\something.torrent, or a magnet link"),
            );
            if ui.button("Add").clicked() {
                let path = self.add_torrent_path.trim().to_string();
                if !path.is_empty() {
                    if path.to_ascii_lowercase().starts_with("magnet:") {
                        self.add_magnet(&path);
                    } else {
                        self.add_torrent(std::path::Path::new(&path));
                    }
                    self.add_torrent_path.clear();
                }
            }
            ui.label(
                egui::RichText::new("or drop a .torrent file onto this window")
                    .size(12.0)
                    .color(egui::Color32::GRAY),
            );
        });
        ui.add_space(4.0);

        let jobs = self.torrents.lock().unwrap().clone();
        if jobs.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.add_space(40.0);
                ui.label(
                    egui::RichText::new("No torrents yet")
                        .size(16.0)
                        .color(egui::Color32::GRAY),
                );
                ui.label(
                    egui::RichText::new(
                        "A .torrent file is read here; a magnet link finds its metadata \
                         through trackers and the DHT. Press Play to watch while it downloads.",
                    )
                    .size(13.0)
                    .color(egui::Color32::GRAY),
                );
            });
            return;
        }

        // Batch bar for torrents, the same shape as the downloads table.
        if !self.checked_torrents.is_empty() {
            let ids: Vec<u64> = self.checked_torrents.iter().copied().collect();
            let n = ids.len();
            let mut action: Option<&str> = None;
            let mut clear = false;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{n} selected"))
                        .size(12.5)
                        .strong(),
                );
                if ui.button("▶ Start").clicked() {
                    action = Some("resume");
                }
                if ui.button("⏸ Pause").clicked() {
                    action = Some("pause");
                }
                if ui.button("↻ Retry").clicked() {
                    action = Some("retry");
                }
                if ui.button("🗑 Delete").clicked() {
                    action = Some("remove");
                }
                if ui.button("Clear").clicked() {
                    clear = true;
                }
            });
            if let Some(a) = action {
                self.torrents_action(&ids, a);
            }
            if clear {
                self.checked_torrents.clear();
            }
            ui.separator();
        }

        ui.horizontal(|ui| {
            let mut all = jobs.iter().all(|j| self.checked_torrents.contains(&j.id));
            if ui
                .add(egui::Checkbox::without_text(&mut all))
                .on_hover_text("Select all torrents")
                .clicked()
            {
                for j in &jobs {
                    if all {
                        self.checked_torrents.insert(j.id);
                    } else {
                        self.checked_torrents.remove(&j.id);
                    }
                }
            }
            ui.label(
                egui::RichText::new(format!("{} torrents", jobs.len()))
                    .size(12.0)
                    .color(egui::Color32::GRAY),
            );
        });
        ui.add_space(2.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            for job in jobs {
                let checked = self.checked_torrents.contains(&job.id);
                egui::Frame::new()
                    .fill(if checked {
                        egui::Color32::from_rgb(31, 41, 59)
                    } else {
                        egui::Color32::from_rgb(24, 26, 32)
                    })
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let mut ticked = checked;
                            if ui.add(egui::Checkbox::without_text(&mut ticked)).changed() {
                                if ticked {
                                    self.checked_torrents.insert(job.id);
                                } else {
                                    self.checked_torrents.remove(&job.id);
                                }
                            }
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&job.name).size(13.0).strong());
                                let fraction = torrent_fraction(&job);
                                // One decimal, and our own text: the built-in
                                // percentage *truncates*, which is how a finished
                                // download came to read "99%".
                                ui.add(
                                    egui::ProgressBar::new(fraction).desired_width(320.0).text(
                                        egui::RichText::new(format!("{:.1}%", fraction * 100.0))
                                            .size(12.0),
                                    ),
                                );
                                let mut detail = match &job.error {
                                    Some(e) => format!("{} · {}", job.state, e),
                                    None => format!(
                                        "{} · {} / {} · {}",
                                        job.state,
                                        fmt_bytes(job.downloaded),
                                        if job.total > 0 {
                                            fmt_bytes(job.total)
                                        } else {
                                            "?".into()
                                        },
                                        if job.speed_bps > 0.0 {
                                            format!("{}/s", fmt_bytes(job.speed_bps as u64))
                                        } else {
                                            "—".into()
                                        }
                                    ),
                                };
                                if job.peers > 0 {
                                    detail.push_str(&format!(" · {} peers", job.peers));
                                }
                                // How long is left is the question a download list
                                // exists to answer, so it is part of the same line.
                                let left = job.total.saturating_sub(job.downloaded);
                                let eta = eta_secs_bytes(left, job.speed_bps);
                                if job.state != "done" && eta > 0 {
                                    detail.push_str(&format!(" · {} left", fmt_secs(eta)));
                                }
                                ui.label(
                                    egui::RichText::new(detail)
                                        .size(11.5)
                                        .color(egui::Color32::GRAY),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    // Rightmost: forgetting the torrent. It is last in
                                    // reading order and first in the right-to-left
                                    // layout, which keeps it away from Stream.
                                    if ui
                                        .button("🗑")
                                        .on_hover_text("Remove this torrent")
                                        .clicked()
                                    {
                                        self.torrents_action(&[job.id], "remove");
                                    }
                                    let state = job.state.as_str();
                                    let active =
                                        matches!(state, "downloading" | "connecting" | "paused");
                                    if state == "failed" || state == "cancelled" {
                                        if ui
                                            .button("↻ Retry")
                                            .on_hover_text("Try this torrent again")
                                            .clicked()
                                        {
                                            self.torrents_action(&[job.id], "retry");
                                        }
                                    } else if active {
                                        if ui.button("Cancel").clicked() {
                                            self.torrents_action(&[job.id], "cancel");
                                        }
                                        let paused = state == "paused";
                                        let toggle = ui.button(if paused {
                                            "▶ Resume"
                                        } else {
                                            "⏸ Pause"
                                        });
                                        if toggle.clicked() {
                                            self.torrents_action(
                                                &[job.id],
                                                if paused { "resume" } else { "pause" },
                                            );
                                        }
                                    }
                                    if !job.files.is_empty()
                                        && matches!(
                                            state,
                                            "downloading" | "connecting" | "done" | "paused"
                                        )
                                    {
                                        let play = ui.button(if state == "done" {
                                            "▶ Play"
                                        } else {
                                            "▶ Stream"
                                        });
                                        let play = play.on_hover_text(if state == "done" {
                                            "Open this torrent in the player"
                                        } else {
                                            "Watch this torrent while it downloads"
                                        });
                                        if play.clicked() {
                                            self.play_torrent(job.id);
                                        }
                                    }
                                    ui.label(
                                        egui::RichText::new(
                                            &job.info_hash[..job.info_hash.len().min(12)],
                                        )
                                        .size(10.5)
                                        .color(egui::Color32::GRAY),
                                    );
                                },
                            );
                        });
                    });
                ui.add_space(6.0);
            }
        });
    }

    /// Start a magnet download on a worker thread.
    fn add_magnet(&mut self, link: &str) {
        match odm::bt::magnet::Magnet::parse(link) {
            Ok(magnet) => self.start_torrent(TorrentSource::Magnet(Box::new(magnet))),
            Err(e) => self.status = format!("magnet link: {e}"),
        }
    }

    /// Start a torrent download on a worker thread.
    fn add_torrent(&mut self, path: &std::path::Path) {
        self.start_torrent(TorrentSource::File(path.to_path_buf()));
    }

    /// The one way a torrent is added, whether it came from a file or a magnet.
    ///
    /// A magnet only knows its hash, so the job starts with no name and no size and
    /// fills them in once the swarm hands over the metadata. That is also the moment
    /// the session becomes playable: the player streams through the live session, so
    /// a film can start minutes before the download ends.
    fn start_torrent(&mut self, source: TorrentSource) {
        let out_dir = Config::load().download_dir;
        std::fs::create_dir_all(&out_dir).ok();

        let (name, info_hash, total, files, output) = match &source {
            TorrentSource::File(path) => match odm::bt::engine::read_torrent(path) {
                Ok(t) => (
                    t.name.clone(),
                    hex::encode(t.info_hash),
                    t.length,
                    t.files
                        .iter()
                        .map(|f| (f.display_name(), f.length))
                        .collect(),
                    odm::bt::engine::output_path(&t, &out_dir),
                ),
                Err(e) => {
                    self.status = format!("{}: {e}", path.display());
                    return;
                }
            },
            TorrentSource::Magnet(m) => (
                m.name.clone().unwrap_or_else(|| m.hex()),
                m.hex(),
                0,
                Vec::new(),
                out_dir.join("magnet"),
            ),
        };

        let id = self.next_torrent_id;
        self.next_torrent_id += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        {
            let mut jobs = self.torrents.lock().unwrap();
            jobs.push(TorrentJob {
                id,
                name,
                info_hash,
                base: out_dir.clone(),
                output,
                total,
                downloaded: 0,
                speed_bps: 0.0,
                peers: 0,
                state: "connecting".into(),
                error: None,
                cancel: cancel.clone(),
                pause: pause.clone(),
                source: Some(source.clone()),
                session: None,
                files,
            });
        }

        spawn_torrent_worker(self.torrents.clone(), id, source, out_dir, cancel, pause);
    }

    /// Pause or resume one torrent without dropping its peers.
    fn set_torrent_paused(&self, id: u64, paused: bool) {
        let mut list = self.torrents.lock().unwrap();
        let Some(job) = list.iter_mut().find(|j| j.id == id) else {
            return;
        };
        job.pause.store(paused, Ordering::SeqCst);
        if let Some(session) = job.session.clone() {
            session.set_paused(paused);
        }
        if paused {
            if job.state != "done" {
                job.state = "paused".into();
            }
        } else if job.state == "paused" {
            job.state = if job.total == 0 {
                "connecting".into()
            } else {
                "downloading".into()
            };
        }
    }

    /// Cancel a torrent: the worker notices the flag and stops the session.
    fn cancel_torrent(&self, id: u64) {
        let mut list = self.torrents.lock().unwrap();
        if let Some(job) = list.iter_mut().find(|j| j.id == id) {
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Forget a torrent and stop everything it owns — the player included.
    fn remove_torrents(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        if let Some(playing) = self.playing_torrent {
            if ids.contains(&playing) {
                self.close_player();
                self.playing_torrent = None;
            }
        }
        {
            let mut list = self.torrents.lock().unwrap();
            for id in ids {
                if let Some(job) = list.iter_mut().find(|j| j.id == *id) {
                    job.cancel.store(true, Ordering::SeqCst);
                }
            }
            list.retain(|j| !ids.contains(&j.id));
        }
        for id in ids {
            self.checked_torrents.remove(id);
        }
        self.status = format!("removed {} torrent(s)", ids.len());
    }

    /// Start a failed or cancelled torrent again from what it was added from.
    fn retry_torrents(&mut self, ids: &[u64]) {
        for id in ids {
            let Some((source, out_dir, cancel, pause)) = ({
                let list = self.torrents.lock().unwrap();
                list.iter().find(|j| j.id == *id).and_then(|j| {
                    // Only a run that has stopped can be started again: retrying a
                    // live torrent would leave two workers writing the same job.
                    if !matches!(j.state.as_str(), "failed" | "cancelled") {
                        return None;
                    }
                    j.source
                        .clone()
                        .map(|s| (s, j.base.clone(), j.cancel.clone(), j.pause.clone()))
                })
            }) else {
                continue;
            };
            // The old worker may still be unwinding; its flag must not stop the new one.
            cancel.store(true, Ordering::SeqCst);
            let cancel = Arc::new(AtomicBool::new(false));
            pause.store(false, Ordering::SeqCst);
            {
                let mut list = self.torrents.lock().unwrap();
                if let Some(job) = list.iter_mut().find(|j| j.id == *id) {
                    job.cancel = cancel.clone();
                    job.session = None;
                    job.error = None;
                    job.state = "connecting".into();
                    job.downloaded = 0;
                    job.speed_bps = 0.0;
                    job.peers = 0;
                }
            }
            spawn_torrent_worker(self.torrents.clone(), *id, source, out_dir, cancel, pause);
        }
        self.status = format!("retrying {} torrent(s)", ids.len());
    }

    /// Apply one action to a set of torrents, for the batch bar.
    fn torrents_action(&mut self, ids: &[u64], action: &str) {
        match action {
            "pause" => {
                for id in ids {
                    self.set_torrent_paused(*id, true);
                }
                self.status = format!("paused {} torrent(s)", ids.len());
            }
            "resume" => {
                for id in ids {
                    self.set_torrent_paused(*id, false);
                }
                self.status = format!("resumed {} torrent(s)", ids.len());
            }
            "cancel" => {
                for id in ids {
                    self.cancel_torrent(*id);
                }
            }
            "retry" => self.retry_torrents(ids),
            "remove" => self.remove_torrents(ids),
            _ => {}
        }
    }

    /// Play a torrent: the local file once it is finished, the live stream before.
    fn play_torrent(&mut self, id: u64) {
        let Some(job) = self
            .torrents
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
        else {
            return;
        };
        // One player at a time. Closing the previous one also stops the server that
        // was feeding it, so the port is not held by a film nobody is watching.
        self.close_player();

        // The biggest video file, or the biggest file if nothing looks like video.
        let file = pick_playable(&job.files);
        let Some((file_name, _)) = job.files.get(file).cloned() else {
            self.status = "this torrent has no files to play".into();
            return;
        };
        self.playing_torrent = Some(id);

        if job.state == "done" {
            // Finished: read straight from the file, no server in the way. The file's
            // name is its path inside the download directory, so a multi-file torrent
            // resolves to `<downloads>/<name>/<subdir>/<file>`.
            let path = {
                let candidate = job.base.join(&file_name);
                if candidate.exists() {
                    candidate
                } else {
                    job.output.clone()
                }
            };
            match player::Player::open(&file_name, &path.to_string_lossy()) {
                Ok(p) => {
                    self.player = Some(p);
                    self.status = format!("playing {}", path.display());
                }
                Err(e) => {
                    self.close_player();
                    self.status = e;
                }
            }
            return;
        }

        // Still downloading: serve the file from the live session and play that.
        let Some(session) = job.session.clone() else {
            self.status = "the torrent is still finding its peers".into();
            return;
        };
        let Some(rt) = stream_rt() else {
            self.status = "could not start the streaming runtime".into();
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let result = match odm::bt::stream::start_with_cancel(session, file, server_stop).await
            {
                Ok(info) => Ok(info.url),
                Err(e) => Err(format!("stream: {e}")),
            };
            let _ = tx.send(result);
        });
        self.stream_stop = Some(stop);
        self.starting_play = Some((rx, file_name));
        self.status = "starting the stream…".into();
    }

    /// Show the player in its own window.
    ///
    /// egui renders an immediate viewport inline, in this frame, into a window of
    /// its own — so the player keeps its state here while the picture, the scrubber
    /// and the transport controls belong to the child window. Closing that window
    /// (its ✕, Alt-F4, or the player's own button) closes the player and, with it,
    /// the stream server feeding it.
    fn player_window(&mut self, ctx: &egui::Context) {
        // Taken out for the duration of the frame: the closure below needs the
        // player mutably, and borrowing it through `self` would also borrow the
        // rest of the interface.
        let Some(mut player) = self.player.take() else {
            return;
        };
        let mut close = false;
        let title = player.title().to_string();

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("odm-player"),
            egui::ViewportBuilder::default()
                .with_title(format!("{title} — Open Download Manager"))
                .with_inner_size([1024.0, 648.0])
                .with_min_inner_size([520.0, 340.0]),
            |ui, _class| {
                // The whole child window is the player: picture, scrubber and
                // transport controls, with no panel furniture around them.
                ui.painter()
                    .rect_filled(ui.max_rect(), 0.0, egui::Color32::BLACK);
                player.pump(ui.ctx());
                if player.ui(ui) {
                    close = true;
                }
                // The window's own close button is the last word on whether the
                // film is still being watched.
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                ui.ctx().request_repaint_after(Duration::from_millis(16));
            },
        );

        if close {
            self.close_player();
        } else {
            self.player = Some(player);
        }
    }

    /// Close the player, and the stream server behind it with it.
    fn close_player(&mut self) {
        self.player = None;
        self.starting_play = None;
        self.playing_torrent = None;
        if let Some(stop) = self.stream_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// Poll the stream server that was being started, and open the player when it
    /// has an address.
    fn poll_starting_play(&mut self) {
        let Some((rx, title)) = &self.starting_play else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(v) => v,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("the streaming server stopped".to_string())
            }
        };
        let title = title.clone();
        self.starting_play = None;
        match outcome {
            Ok(url) => match player::Player::open(&title, &url) {
                Ok(p) => {
                    self.player = Some(p);
                    self.status = format!("playing {title}");
                }
                Err(e) => {
                    self.close_player();
                    self.status = e;
                }
            },
            Err(e) => {
                self.close_player();
                self.status = e;
            }
        }
    }

    // ----- status bar -----------------------------------------------------
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(24, 26, 32))
            .inner_margin(egui::Margin::symmetric(12, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let active = self.jobs.iter().filter(|j| j.state == "active").count();
                    let done = self.jobs.iter().filter(|j| j.state == "done").count();
                    let total_speed: f64 = self.jobs.iter().map(|j| j.speed_bps).sum();
                    ui.label(
                        egui::RichText::new(format!("{active} active · {done} finished"))
                            .size(12.0)
                            .color(egui::Color32::GRAY),
                    );
                    if total_speed > 0.0 {
                        ui.label(
                            egui::RichText::new(format!("· {} /s", fmt_bytes(total_speed as u64)))
                                .size(12.0)
                                .color(egui::Color32::GRAY),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(limit) = self.quota.limit {
                            let pct = (self.quota.used as f64 / limit as f64).min(1.0) as f32;
                            ui.label(
                                egui::RichText::new(format!(
                                    "Quota {} / {}",
                                    fmt_bytes(self.quota.used),
                                    fmt_bytes(limit)
                                ))
                                .size(12.0)
                                .color(egui::Color32::GRAY),
                            );
                            ui.add(
                                egui::ProgressBar::new(pct)
                                    .desired_width(140.0)
                                    .show_percentage(),
                            );
                            if let Some(r) = self.quota.resets_in_secs {
                                ui.label(
                                    egui::RichText::new(format!("refills in {r}s"))
                                        .size(11.0)
                                        .color(egui::Color32::GRAY),
                                );
                            }
                        }
                    });
                });
                if !self.status.is_empty() {
                    ui.label(
                        egui::RichText::new(&self.status)
                            .size(11.5)
                            .color(egui::Color32::from_rgb(148, 163, 184)),
                    );
                }
            });
    }

    // ----- add dialog -----------------------------------------------------
    fn add_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_add;
        egui::Window::new("Add URL")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Address");
                ui.add_sized(
                    [460.0, 26.0],
                    egui::TextEdit::singleline(&mut self.url_input)
                        .hint_text("https://host/file.iso"),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Connections");
                    let mut n = self.add_connections.unwrap_or(self.default_connections);
                    if ui.add(egui::Slider::new(&mut n, 1..=32)).changed() {
                        self.add_connections = Some(n);
                    } else {
                        self.add_connections = None;
                    }
                    if self.add_connections.is_some() && ui.small_button("default").clicked() {
                        self.add_connections = None;
                    }
                });
                ui.label("Save as (optional)");
                ui.add_sized(
                    [460.0, 26.0],
                    egui::TextEdit::singleline(&mut self.add_output).hint_text("choose a filename"),
                );
                ui.label("Quality (optional, for HLS/DASH)");
                ui.add_sized(
                    [460.0, 26.0],
                    egui::TextEdit::singleline(&mut self.add_quality).hint_text("best, 1080p, …"),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        self.add_job();
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_add = false;
                    }
                });
            });
        self.show_add = open;
    }
}

fn job_icon(j: &Job) -> Lucide {
    if is_audio(j) {
        Lucide::Music
    } else if is_video(j) {
        Lucide::Film
    } else if is_archive(j) {
        Lucide::FileArchive
    } else if is_program(j) {
        Lucide::Package
    } else {
        Lucide::FileText
    }
}

fn job_description(j: &Job) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(j.kind.clone());
    if j.connections > 1 {
        parts.push(format!("{} conn", j.connections));
    }
    if j.resumable {
        parts.push("resumable".into());
    }
    if let Some(c) = &j.checksum {
        parts.push(format!("{} verified", c.algorithm.to_ascii_lowercase()));
    }
    if let Some(e) = job_error(j) {
        parts.push(e);
    }
    parts.join(" · ")
}

fn job_error(j: &Job) -> Option<String> {
    j.error.clone()
}

/// A short "last try" label: how long ago the job last changed.
fn format_time(ms: u64) -> String {
    if ms == 0 {
        return "—".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(ms);
    let delta = now.saturating_sub(ms) / 1000;
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    // `--help` and `--version` must not start a daemon: a question should not leave a
    // background process behind.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Open Download Manager\n\n  USAGE:\n    odm\n\n  A native download manager: HTTP/HTTPS with resume, a download\n  quota, checksum verification, HLS/DASH capture, recurring projects, offline\n  site archiving, and BitTorrent with magnet links.\n\n  The daemon it needs is started with it; the command-line tools\n  `odm-cli` and `odm-bt` are installed alongside."
        );
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("odm {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // Start the daemon in-process so the app is standalone (no separate daemon needed).
    //
    // The configured address is tried first; if the port is taken — and it often is,
    // because Windows reserves a scattering of them and WSL2 hands some back to the
    // host — the app falls back to an ephemeral port and talks to that instead. A
    // download manager that refuses to start because 1001 is busy would be a poor
    // neighbour to the browser extension, which expects 1001.
    let mut cfg = Config::load();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let bound = match odm::daemon::bind(&cfg).await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!(
                        "cannot bind {} ({e}); trying an ephemeral port",
                        cfg.daemon_addr
                    );
                    cfg.daemon_addr = "127.0.0.1:0".to_string();
                    match odm::daemon::bind(&cfg).await {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("cannot start the daemon: {e}");
                            return;
                        }
                    }
                }
            };
            let (listener, sched, _stop) = bound;
            let addr = listener
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            let _ = DAEMON_ADDR.set(format!("http://{addr}"));
            let auth = odm::auth::token::load(None, cfg.daemon_token_file.as_deref())
                .ok()
                .flatten()
                .map(std::sync::Arc::new);
            let _ = odm::daemon::serve_on(listener, sched, auth).await;
        });
    });
    std::thread::sleep(Duration::from_millis(600));

    let mut style = egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    };
    style.visuals.panel_fill = egui::Color32::from_rgb(18, 20, 25);
    style.visuals.window_fill = egui::Color32::from_rgb(24, 26, 32);
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(12, 13, 17);
    style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(30, 33, 40);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(45, 50, 60);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(37, 99, 235);
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(30, 33, 40);
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(37, 99, 235).linear_multiply(0.6);
    style.visuals.hyperlink_color = egui::Color32::from_rgb(96, 165, 250);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(12);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 740.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("Open Download Manager")
            // The window's own icon, so the task bar does not show egui's default.
            .with_icon(
                eframe::icon_data::from_png_bytes(&include_bytes!("../assets/odm.png")[..])
                    .expect("the icon is a PNG this program wrote itself"),
            ),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Open Download Manager",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            cc.egui_ctx.set_style_of(egui::Theme::Dark, style);
            Ok(Box::new(App::new()))
        }),
    );
}

// ---------------------------------------------------------------------------
// Torrent helpers
// ---------------------------------------------------------------------------

/// Mark a torrent download as failed, with the reason.
/// What a torrent was added from.
#[derive(Clone)]
enum TorrentSource {
    /// A `.torrent` file on disk.
    File(PathBuf),
    /// A magnet link, which still has to find its metadata.
    Magnet(Box<odm::bt::magnet::Magnet>),
}

/// Run one torrent on its own thread and runtime.
///
/// Shared by the first attempt and by Retry: a torrent that failed while the swarm
/// was unreachable is the same torrent, and asking the user to find the `.torrent`
/// again to try once more would be a strange thing to require.
fn spawn_torrent_worker(
    jobs: Arc<std::sync::Mutex<Vec<TorrentJob>>>,
    id: u64,
    source: TorrentSource,
    out_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                set_torrent_error(&jobs, id, &format!("{e}"));
                return;
            }
        };
        rt.block_on(async move {
            let progress = progress_fn(&jobs, id);
            let opened = match source {
                TorrentSource::File(path) => match odm::bt::engine::read_torrent(&path) {
                    Ok(t) => {
                        let torrent = Arc::new(t);
                        odm::bt::engine::open_torrent(
                            torrent,
                            out_dir.clone(),
                            Some(progress),
                            Some(cancel.clone()),
                        )
                        .await
                        .map(|s| (s.torrent.clone(), s))
                    }
                    Err(e) => Err(e),
                },
                TorrentSource::Magnet(m) => {
                    odm::bt::engine::open_magnet(&m, &out_dir, Some(progress), Some(cancel.clone()))
                        .await
                }
            };

            let (torrent, session) = match opened {
                Ok(v) => v,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("cancelled") {
                        set_torrent_state(&jobs, id, "cancelled");
                    } else {
                        set_torrent_error(&jobs, id, &msg);
                    }
                    return;
                }
            };

            publish_metadata(&jobs, id, &torrent, &session, &out_dir);
            // A pause asked for while the metadata was still being fetched holds now
            // that there is a session to hold it.
            if pause.load(Ordering::SeqCst) {
                session.set_paused(true);
                set_torrent_state(&jobs, id, "paused");
            }

            match session.wait().await {
                Ok(()) => set_torrent_state(&jobs, id, "done"),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("cancelled") {
                        set_torrent_state(&jobs, id, "cancelled");
                    } else {
                        set_torrent_error(&jobs, id, &msg);
                    }
                }
            }
        });
    });
}

fn set_torrent_error(jobs: &Arc<std::sync::Mutex<Vec<TorrentJob>>>, id: u64, msg: &str) {
    if let Ok(mut list) = jobs.lock() {
        if let Some(job) = list.iter_mut().find(|j| j.id == id) {
            job.state = "failed".into();
            job.error = Some(msg.to_string());
        }
    }
}

/// Move a torrent download to a new state.
fn set_torrent_state(jobs: &Arc<std::sync::Mutex<Vec<TorrentJob>>>, id: u64, state: &str) {
    if let Ok(mut list) = jobs.lock() {
        if let Some(job) = list.iter_mut().find(|j| j.id == id) {
            job.state = state.to_string();
            if state == "done" {
                // The last progress callback may have been a second before the
                // final piece verified; a finished job is finished.
                if job.total > 0 {
                    job.downloaded = job.total;
                }
                job.speed_bps = 0.0;
            }
        }
    }
}

/// Report the session's progress into the list the interface draws from.
fn progress_fn(
    jobs: &Arc<std::sync::Mutex<Vec<TorrentJob>>>,
    id: u64,
) -> odm::bt::session::ProgressFn {
    let jobs = jobs.clone();
    Arc::new(move |p: odm::bt::session::TorrentProgress| {
        if let Ok(mut list) = jobs.lock() {
            if let Some(job) = list.iter_mut().find(|j| j.id == id) {
                job.downloaded = p.downloaded;
                job.total = p.total;
                job.speed_bps = p.speed_bps;
                job.peers = p.peers;
            }
        }
    })
}

/// The metadata is in: fill in the name, the size and the files, and hand the live
/// session to the interface so it can be streamed.
fn publish_metadata(
    jobs: &Arc<std::sync::Mutex<Vec<TorrentJob>>>,
    id: u64,
    torrent: &odm::bt::torrent::Torrent,
    session: &Arc<odm::bt::session::Session>,
    out_dir: &std::path::Path,
) {
    if let Ok(mut list) = jobs.lock() {
        if let Some(job) = list.iter_mut().find(|j| j.id == id) {
            job.name = torrent.name.clone();
            job.total = torrent.length;
            job.files = torrent
                .files
                .iter()
                .map(|f| (f.display_name(), f.length))
                .collect();
            job.output = odm::bt::engine::output_path(torrent, out_dir);
            // The player streams through this session, so the interface has to hold
            // on to it; the worker thread only keeps the download running.
            job.session = Some(session.clone());
            if job.state == "connecting" {
                job.state = "downloading".into();
            }
        }
    }
}

/// Which file in a torrent a player should open: the biggest video, or — for a
/// torrent of music, say — the biggest file of any kind.
fn pick_playable(files: &[(String, u64)]) -> usize {
    const VIDEO: &[&str] = &[
        "mp4", "m4v", "mkv", "webm", "avi", "mov", "ts", "m2ts", "mpg", "mpeg", "flv", "wmv",
    ];
    let video = files
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| {
            name.rsplit('.')
                .next()
                .map(|ext| VIDEO.contains(&ext.to_ascii_lowercase().as_str()))
                .unwrap_or(false)
        })
        .max_by_key(|(_, (_, size))| *size)
        .map(|(i, _)| i);
    video.unwrap_or_else(|| {
        files
            .iter()
            .enumerate()
            .max_by_key(|(_, (_, size))| *size)
            .map(|(i, _)| i)
            .unwrap_or(0)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// The interface is tested the way a person uses it — a click at a point — but
// headlessly and deterministically: no display, no mouse, no timing, and the
// same result every run.
//
// The controls are found by *name* rather than by coordinates: the window
// records where it drew each one, so a test says "click Torrents" and not
// "click 519,215". A layout change moves the buttons; it does not break the
// test.
//
// The assertions are about the end state ("the Torrents tab is showing"), not
// about the click having happened — which is the difference between a test and
// a script that hopes.

#[cfg(test)]
mod tests {
    use super::*;

    /// An application with no daemon behind it, and no jobs.
    fn app() -> App {
        App {
            default_connections: 8,
            ..Default::default()
        }
    }

    /// Draw one frame.
    fn frame(app: &mut App, ctx: &egui::Context, input: egui::RawInput) {
        let mut out = ctx.run_ui(input, |ui| app.draw(ui));
        // A test has no renderer to hand the icon textures to, and an unapplied
        // texture delta is an error rather than a leak.
        out.textures_delta.clear();
    }

    /// A click at a point: the pointer arrives, the button goes down, it comes up.
    fn click_at(pos: egui::Pos2) -> egui::RawInput {
        egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            ..Default::default()
        }
    }

    /// Click a control by name.
    ///
    /// The rectangle comes from the frame that was just drawn, so the test
    /// always clicks where the control *is*, not where it used to be.
    fn click(app: &mut App, ctx: &egui::Context, name: &str) {
        frame(app, ctx, egui::RawInput::default());
        let rect = *app
            .rects
            .get(name)
            .unwrap_or_else(|| panic!("there is no control called {name:?}"));
        let pos = rect.center();
        frame(app, ctx, click_at(pos));
    }

    #[test]
    fn the_tabs_switch_and_the_strip_is_always_there() {
        let mut app = app();
        let ctx = egui::Context::default();

        // The strip is drawn in the frame, not inside a tab: it must be there
        // whatever tab is showing.
        for tab in [Tab::Downloads, Tab::Projects, Tab::Torrents] {
            app.tab = tab;
            frame(&mut app, &ctx, egui::RawInput::default());
            for name in ["tab.downloads", "tab.projects", "tab.torrents"] {
                assert!(
                    app.rects.contains_key(name),
                    "the {name:?} tab is missing while {tab:?} is showing"
                );
            }
        }

        // And clicking one switches to it.
        click(&mut app, &ctx, "tab.torrents");
        assert_eq!(app.tab, Tab::Torrents);
        click(&mut app, &ctx, "tab.downloads");
        assert_eq!(app.tab, Tab::Downloads);
    }

    #[test]
    fn the_add_url_button_opens_its_dialog() {
        let mut app = app();
        let ctx = egui::Context::default();
        assert!(!app.show_add, "the dialog starts closed");
        click(&mut app, &ctx, "tool.Add URL");
        assert!(app.show_add, "the Add URL button did not open the dialog");
    }

    #[test]
    fn print_control_rects() {
        let mut app = app();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1536.0, 792.0),
            )),
            ..Default::default()
        };
        frame(&mut app, &ctx, input);
        let mut rects: Vec<_> = app.rects.iter().collect();
        rects.sort_by(|a, b| a.0.cmp(b.0));
        for (name, rect) in rects {
            println!(
                "{name}: center=({:.1},{:.1}) rect=({:.1},{:.1})-({:.1},{:.1})",
                rect.center().x,
                rect.center().y,
                rect.min.x,
                rect.min.y,
                rect.max.x,
                rect.max.y
            );
        }
    }

    /// A torrent job with nothing but the fields a progress bar reads.
    fn a_torrent(state: &str, downloaded: u64, total: u64) -> TorrentJob {
        TorrentJob {
            id: 1,
            name: "t".into(),
            info_hash: "0".repeat(40),
            base: std::env::temp_dir(),
            output: std::env::temp_dir().join("t.mkv"),
            total,
            downloaded,
            speed_bps: 0.0,
            peers: 0,
            state: state.into(),
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            source: None,
            session: None,
            files: Vec::new(),
        }
    }

    #[test]
    fn a_finished_torrent_reads_one_hundred_percent() {
        // The progress bar's built-in text truncates, so the last tick before the
        // final piece verified stayed on screen as "99%" over a finished download.
        let nearly = torrent_fraction(&a_torrent("downloading", 999, 1000));
        assert!((nearly - 0.999).abs() < 1e-6, "{nearly}");
        assert_eq!(torrent_fraction(&a_torrent("downloading", 1000, 1000)), 1.0);
        assert_eq!(torrent_fraction(&a_torrent("done", 999, 1000)), 1.0);
        // A magnet before its metadata: no size, so no honest fraction.
        assert_eq!(torrent_fraction(&a_torrent("connecting", 0, 0)), 0.0);
    }

    #[test]
    fn finishing_a_torrent_marks_it_complete() {
        let jobs = Arc::new(std::sync::Mutex::new(vec![a_torrent(
            "downloading",
            990,
            1000,
        )]));
        set_torrent_state(&jobs, 1, "done");
        let list = jobs.lock().unwrap();
        assert_eq!(list[0].downloaded, 1000, "a done job is a complete job");
        assert_eq!(list[0].speed_bps, 0.0);
        assert_eq!(torrent_fraction(&list[0]), 1.0);
    }

    #[test]
    fn pausing_a_torrent_is_reversible_without_losing_its_place() {
        let jobs = Arc::new(std::sync::Mutex::new(vec![a_torrent(
            "downloading",
            400,
            1000,
        )]));
        let app = App {
            torrents: jobs.clone(),
            ..Default::default()
        };

        app.set_torrent_paused(1, true);
        {
            let list = jobs.lock().unwrap();
            assert_eq!(list[0].state, "paused");
            assert!(list[0].pause.load(Ordering::SeqCst));
            assert_eq!(list[0].downloaded, 400, "a pause is not a reset");
        }

        app.set_torrent_paused(1, false);
        let list = jobs.lock().unwrap();
        assert_eq!(list[0].state, "downloading");
        assert!(!list[0].pause.load(Ordering::SeqCst));
        assert_eq!(list[0].downloaded, 400);
    }

    #[test]
    fn a_magnet_that_has_no_size_yet_returns_to_connecting() {
        let jobs = Arc::new(std::sync::Mutex::new(vec![a_torrent("paused", 0, 0)]));
        let app = App {
            torrents: jobs.clone(),
            ..Default::default()
        };
        app.set_torrent_paused(1, false);
        assert_eq!(jobs.lock().unwrap()[0].state, "connecting");
    }

    #[test]
    fn deleting_a_batch_forgets_every_ticked_torrent() {
        let mut second = a_torrent("downloading", 10, 100);
        second.id = 2;
        let jobs = Arc::new(std::sync::Mutex::new(vec![
            a_torrent("downloading", 10, 100),
            second,
        ]));
        let mut app = App {
            torrents: jobs.clone(),
            checked_torrents: BTreeSet::from([1, 2]),
            ..Default::default()
        };
        app.remove_torrents(&[1, 2]);
        assert!(jobs.lock().unwrap().is_empty());
        assert!(app.checked_torrents.is_empty());
    }

    #[test]
    fn the_time_left_follows_the_rate() {
        let mib = 1024.0 * 1024.0;
        assert_eq!(fmt_secs(eta_secs_bytes(10 * 1024 * 1024, mib)), "10s");
        assert_eq!(fmt_secs(eta_secs_bytes(90 * 1024 * 1024, mib)), "1m 30s");
        assert_eq!(fmt_secs(eta_secs_bytes(3661, 1.0)), "1h 01m");
        // No rate, or nothing left, is not an estimate: saying "0s" would lie.
        assert_eq!(eta_secs_bytes(1000, 0.0), 0);
        assert_eq!(eta_secs_bytes(0, 5.0), 0);
    }

    #[test]
    fn a_single_download_can_be_ticked_without_selecting_the_row() {
        // The row claims clicks for selection and is registered *after* the
        // checkbox, so its interact used to cover the checkbox too: the tick was
        // swallowed, and only "select all" — which has no row interact over it —
        // worked.
        let mut app = app();
        app.tab = Tab::Downloads;
        app.daemon_ok = true;
        // The first frame normally asks the daemon for the job list, which would
        // replace the fixture with an empty one.
        app.last_refresh = Some(std::time::Instant::now());
        app.jobs = vec![Job {
            id: "a".into(),
            output: "/tmp/movie.mkv".into(),
            state: "done".into(),
            ..Default::default()
        }];
        let ctx = egui::Context::default();
        click(&mut app, &ctx, "job.check.a");
        assert!(app.checked.contains("a"), "the checkbox did not tick");
        assert!(
            app.focused.is_none(),
            "ticking the checkbox also selected the row"
        );
        click(&mut app, &ctx, "job.check.a");
        assert!(!app.checked.contains("a"), "the second click did not untick");
    }

    #[test]
    fn a_category_filters_the_list() {
        let mut app = app();
        app.jobs = vec![
            Job {
                id: "a".into(),
                output: "/tmp/movie.mkv".into(),
                state: "done".into(),
                ..Default::default()
            },
            Job {
                id: "b".into(),
                output: "/tmp/notes.txt".into(),
                state: "done".into(),
                ..Default::default()
            },
        ];
        app.category = Category::All;
        assert_eq!(app.visible_jobs().len(), 2);
        app.category = Category::Video;
        assert_eq!(app.visible_jobs().len(), 1, "only the .mkv is a video");
        app.category = Category::Documents;
        assert_eq!(app.visible_jobs().len(), 1, "only the .txt is a document");
    }
}
