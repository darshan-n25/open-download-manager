// Open Download Manager — the native desktop app (egui/eframe).
//
// Standalone: starts the riptide daemon in-process on 127.0.0.1:1001, so the user
// just runs the exe (like IDM) without launching a separate daemon. The UI talks
// to the daemon's HTTP API.
//
// Layout follows the brief: a menu bar, a toolbar of icon actions, a category
// tree down the left, and a sortable jobs table in the middle — but styled dark
// and flat rather than the 2000s-era grey.

use eframe::egui;
use egui_lucide::Lucide;
use riptide::config::Config;
use std::sync::OnceLock;
use std::time::Duration;

/// The base URL of the in-process daemon, filled in once it has bound.
static DAEMON_ADDR: OnceLock<String> = OnceLock::new();


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
        .map(|s| s.clone())
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
    let name = j.output.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("");
    match name.rfind('.') {
        Some(i) => name[i + 1..].to_ascii_lowercase(),
        None => String::new(),
    }
}

fn is_doc(j: &Job) -> bool {
    let e = ext_of(j);
    ["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "rtf", "odt", "epub"]
        .contains(&e.as_str())
}

fn is_archive(j: &Job) -> bool {
    let e = ext_of(j);
    ["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "dmg", "cab"].contains(&e.as_str())
}

fn is_audio(j: &Job) -> bool {
    let e = ext_of(j);
    ["mp3", "flac", "wav", "aac", "ogg", "m4a", "wma", "opus", "aiff"].contains(&e.as_str())
}

fn is_video(j: &Job) -> bool {
    let e = ext_of(j);
    ["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "m3u8"]
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
        SortState { by: SortBy::Name, asc: true }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Downloads,
    Projects,
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
    selected: Option<String>,
    sort: SortState,
    tab: Tab,
    show_add: bool,
    /// `None` means "whatever the daemon is configured to use". A slider would silently
    /// clamp a zero into one, and a download with one connection is a fifth of the
    /// speed — so the number is only sent when the user actually asks for one.
    add_connections: Option<usize>,
    add_output: String,
    add_quality: String,
    search: String,
    /// The daemon's configured number of connections, shown in the Add URL dialog.
    default_connections: usize,
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
                SortBy::Name => a.output.to_ascii_lowercase().cmp(&b.output.to_ascii_lowercase()),
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
        let id = self.selected.as_ref()?;
        self.jobs.iter().find(|j| &j.id == id)
    }

    fn act(&mut self, id: &str, action: &str) {
        if let Some(t) = http_post(&format!("/api/jobs/{id}/{action}"), "{}") {
            self.status = t;
        }
        self.refresh();
    }

    fn act_all(&mut self, action: &str) {
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
    if j.speed_bps <= 0.0 {
        return 0;
    }
    let Some(t) = j.total else { return 0 };
    let left = t.saturating_sub(j.downloaded);
    (left as f64 / j.speed_bps) as u64
}

fn fmt_eta(j: &Job) -> String {
    if j.state.eq_ignore_ascii_case("done") {
        return String::new();
    }
    if j.speed_bps <= 0.0 {
        return "—".into();
    }
    let s = eta_secs(j);
    if s == 0 {
        return "—".into();
    }
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
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

        egui::Panel::left("categories")
            .exact_size(200.0)
            .resizable(false)
            .show(ui, |ui| self.categories(ui));

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            match self.tab {
                Tab::Downloads => self.downloads(ui),
                Tab::Projects => self.projects_tab(ui),
            }
        });

        if self.show_add {
            self.add_dialog(&ctx);
        }

        ctx.request_repaint_after(Duration::from_millis(1200));
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
                if ui.selectable_value(&mut t, Tab::Downloads, "Downloads").clicked() {
                    self.tab = t;
                    ui.close();
                }
                if ui.selectable_value(&mut t, Tab::Projects, "Projects").clicked() {
                    self.tab = t;
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About Open Download Manager").clicked() {
                    self.status = "Open Download Manager 0.1.0 — an open-source download manager".into();
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
                    ui.spacing_mut().item_spacing.x = 14.0;

                    self.tool_button(ui, Lucide::Plus, "Add URL", egui::Color32::from_rgb(96, 165, 250), |s| {
                        s.show_add = true;
                    });
                    self.tool_button(ui, Lucide::Play, "Resume", egui::Color32::from_rgb(74, 222, 128), |s| {
                        s.act_all("resume");
                    });
                    self.tool_button(ui, Lucide::Square, "Stop", egui::Color32::from_rgb(248, 113, 113), |s| {
                        s.act_all("pause");
                    });
                    self.tool_button(ui, Lucide::Trash, "Delete", egui::Color32::from_rgb(148, 163, 184), |s| {
                        if let Some(id) = s.selected.clone() {
                            s.act(&id, "remove");
                        }
                    });
                    self.tool_button(ui, Lucide::Trash2, "Delete Completed", egui::Color32::from_rgb(148, 163, 184), |s| {
                        s.delete_finished();
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    self.tool_button(ui, Lucide::Settings, "Options", egui::Color32::from_rgb(167, 139, 250), |s| {
                        s.status =
                            "Set limits with `riptide config set` (speed_limit, quota_bytes)".into();
                    });
                    self.tool_button(ui, Lucide::Clock, "Scheduler", egui::Color32::from_rgb(251, 191, 36), |s| {
                        s.tab = Tab::Projects;
                    });
                    self.tool_button(ui, Lucide::CirclePlay, "Start Queue", egui::Color32::from_rgb(96, 165, 250), |s| {
                        s.queue_start();
                    });
                    self.tool_button(ui, Lucide::CircleStop, "Stop Queue", egui::Color32::from_rgb(248, 113, 113), |s| {
                        s.queue_stop();
                    });

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
        // The icon and the caption are two real buttons stacked on top of each other.
        // An image and a label do not react to a click on their own, and a rectangle
        // allocated by hand does not reliably get the pointer either — egui's own
        // widgets are the ones that know how to be hovered and clicked.
        //
        // The style is scoped to this button: a transparent plate until the pointer
        // arrives, a soft one while it is there, the accent while it is pressed.
        let mut style = ui.style().as_ref().clone();
        style.visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(58, 66, 82);
        style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(37, 99, 235);
        style.spacing.button_padding = egui::vec2(4.0, 2.0);

        let mut clicked = false;
        ui.scope(|ui| {
            ui.set_style(style);
            ui.vertical(|ui| {
                ui.set_max_width(66.0);
                let icon_response = ui.add(
                    egui::Button::new(icon.size(22.0).color(color).image())
                        .min_size(egui::vec2(28.0, 26.0)),
                );
                let label_response = ui.add(
                    egui::Button::new(egui::RichText::new(label).size(11.0))
                        .min_size(egui::vec2(64.0, 18.0)),
                );
                clicked = icon_response.clicked() || label_response.clicked();
            });
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
                ui.scope_builder(
                    egui::UiBuilder::new().max_rect(rect),
                    |ui| {
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
                    },
                );
                if response.clicked() {
                    self.category = cat;
                }
            };

            category_row(ui, Category::All);
            ui.add_space(4.0);
            for cat in [
                Category::Unfinished,
                Category::Finished,
                Category::Failed,
            ] {
                category_row(ui, cat);
            }
            ui.add_space(8.0);
            ui.label(egui::RichText::new("CATEGORIES").size(10.0).color(egui::Color32::GRAY));
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
            ui.label(egui::RichText::new("OTHER").size(10.0).color(egui::Color32::GRAY));
            ui.add_space(2.0);
            category_row(ui, Category::Projects);
        });
    }

    // ----- downloads table -------------------------------------------------
    fn downloads(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let t = self.tab;
            if ui.selectable_label(t == Tab::Downloads, "Downloads").clicked() {
                self.tab = t;
            }
            if ui.selectable_label(t == Tab::Projects, "Projects").clicked() {
                self.tab = t;
            }
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
                        let mut header = |ui: &mut egui::Ui, label: &str, by: SortBy, width: f32| {
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
                        header(ui, "File Name", SortBy::Name, 232.0);
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
                let selected = self.selected.as_deref() == Some(job.id.as_str());
                let bg = if selected {
                    egui::Color32::from_rgb(37, 99, 235)
                } else {
                    egui::Color32::from_rgb(24, 26, 32)
                };
                let row = egui::Frame::new()
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(8, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.horizontal(|ui| {
                                ui.add(job_icon(&job).size(16.0).color(state_color(&job.state)));
                                ui.vertical(|ui| {
                                    ui.add_sized(
                                        [228.0, 18.0],
                                        egui::Label::new(
                                            egui::RichText::new(
                                                job.output
                                                    .rsplit(|c| c == '/' || c == '\\')
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
                                                .desired_width(224.0)
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
                // itself — otherwise the job list cannot be selected at all.
                let resp = ui.interact(
                    row.response.rect,
                    ui.id().with(("job", job.id.clone())),
                    egui::Sense::click(),
                );
                if resp.hovered() && !selected {
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
                    self.selected = Some(job.id.clone());
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
                                egui::RichText::new(e).color(egui::Color32::from_rgb(248, 113, 113)),
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
        ui.horizontal(|ui| {
            let t = self.tab;
            if ui.selectable_label(t == Tab::Downloads, "Downloads").clicked() {
                self.tab = t;
            }
            if ui.selectable_label(t == Tab::Projects, "Projects").clicked() {
                self.tab = t;
            }
        });
        ui.separator();
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
                        "Create one with `riptide project add` — a file kept in sync, a site \
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
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Run now").clicked() {
                                    let _ = http_post(&format!("/api/projects/{}/run", p.name), "{}");
                                    self.refresh();
                                }
                            });
                        });
                    });
                ui.add_space(6.0);
            }
        });
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
                            ui.add(egui::ProgressBar::new(pct).desired_width(140.0).show_percentage());
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
                    if ui
                        .add(egui::Slider::new(&mut n, 1..=32))
                        .changed()
                    {
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
            let bound = match riptide::daemon::bind(&cfg).await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("cannot bind {} ({e}); trying an ephemeral port", cfg.daemon_addr);
                    cfg.daemon_addr = "127.0.0.1:0".to_string();
                    match riptide::daemon::bind(&cfg).await {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("cannot start the daemon: {e}");
                            return;
                        }
                    }
                }
            };
            let (listener, sched, _stop) = bound;
            let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
            let _ = DAEMON_ADDR.set(format!("http://{addr}"));
            let auth = riptide::auth::token::load(None, cfg.daemon_token_file.as_deref())
                .ok()
                .flatten()
                .map(std::sync::Arc::new);
            let _ = riptide::daemon::serve_on(listener, sched, auth).await;
        });
    });
    std::thread::sleep(Duration::from_millis(600));

    let mut style = egui::Style::default();
    style.visuals = egui::Visuals::dark();
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
            .with_icon(eframe::icon_data::from_png_bytes(
                &include_bytes!("../assets/odm.png")[..],
            )
            .expect("the icon is a PNG this program wrote itself")),
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
