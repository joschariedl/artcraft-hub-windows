#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
//! ArtCraft Hub – a native installer/updater for the Craft Apps.
//!
//! No web view, no Electron: egui/eframe render the whole UI. Every install is
//! verified against the release's `SHA256SUMS.txt` and VirusTotal before a
//! single byte is extracted.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, Vec2,
};

use artcraft_core::catalog::{self};
use artcraft_core::config::Config;
use artcraft_core::ops::{self, AppStatus};
use artcraft_core::paths::Paths;
use artcraft_core::progress::Progress;
use artcraft_core::vt::{self, VtCache};

use artcraft_core::theme::Theme;
use std::sync::OnceLock;

struct Palette {
    bg: Color32,
    panel: Color32,
    card: Color32,
    card_hover: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
    ok: Color32,
    warn: Color32,
    bad: Color32,
}

impl Palette {
    fn from_theme(theme: &Theme) -> Self {
        let to = |c: [u8; 3]| Color32::from_rgb(c[0], c[1], c[2]);
        let card = theme.card;
        let card_hover = artcraft_core::theme::mix(card, theme.text, 0.08);
        Self {
            bg: to(theme.background),
            panel: to(theme.panel),
            card: to(theme.card),
            card_hover: to(card_hover),
            border: to(theme.border),
            text: to(theme.text),
            muted: to(theme.muted),
            accent: to(theme.accent),
            ok: to(theme.ok),
            warn: to(theme.warn),
            bad: to(theme.bad),
        }
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

fn palette() -> &'static Palette {
    PALETTE.get_or_init(|| Palette::from_theme(&Theme::default()))
}

fn set_palette(theme: &Theme) {
    let _ = PALETTE.set(Palette::from_theme(theme));
}

fn bg() -> Color32 { palette().bg }
fn panel() -> Color32 { palette().panel }
fn card() -> Color32 { palette().card }
fn card_hover() -> Color32 { palette().card_hover }
fn border() -> Color32 { palette().border }
fn text() -> Color32 { palette().text }
fn muted() -> Color32 { palette().muted }
fn accent() -> Color32 { palette().accent }
fn ok() -> Color32 { palette().ok }
fn warn() -> Color32 { palette().warn }
fn bad() -> Color32 { palette().bad }

fn main() -> eframe::Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("artcraft-hub {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!(
                    "ArtCraft Hub {}\n\nUsage: artcraft-hub\n\nTakes no arguments; all actions happen in the UI.\n\n  -h, --help     This help\n  -V, --version  Print version",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            _ => {
                std::process::exit(2);
            }
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ArtCraft Hub")
            .with_inner_size([1120.0, 780.0])
            .with_min_inner_size([900.0, 620.0])
            .with_app_id("artcraft-hub"),
        ..Default::default()
    };
    eframe::run_native(
        "artcraft-hub",
        options,
        Box::new(|cc| Ok(Box::new(HubApp::new(cc)))), 
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Apps,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Filter {
    #[default]
    All,
    Updates,
    Installed,
    NotInstalled,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Updates => "Updates",
            Filter::Installed => "Installed",
            Filter::NotInstalled => "Not installed",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Busy {
    Check,
    Install(String),
    InstallAll,
    Upgrade,
}

impl Busy {
    fn label(&self) -> String {
        match self {
            Busy::Check => "Checking releases …".into(),
            Busy::Install(name) => format!("Installing {name} …"),
            Busy::InstallAll => "Installing apps …".into(),
            Busy::Upgrade => "Updating apps …".into(),
        }
    }
}

enum Msg {
    Status(Vec<AppStatus>),
    Progress(Progress),
    Done { text: String, ok: bool },
}

struct SettingsDraft {
    vt_key: String,
    github_token: String,
    allow_unknown: bool,
    install_1024: bool,
    require_sums: bool,
    check_interval: u64,
    keep_versions: u64,
    parallel_checks: u64,
    network_retries: u64,
    follow_omarchy_theme: bool,
    require_attestations: bool,
    require_signed_manifest: bool,
    require_gpg_signature: bool,
    gpg_keyring: String,
    gpg_trusted_fingerprints: String,
}

impl SettingsDraft {
    fn from_config(cfg: &Config) -> Self {
        Self {
            vt_key: cfg.virustotal_api_key.clone(),
            github_token: cfg.github_token.clone(),
            allow_unknown: cfg.vt.allow_unknown,
            install_1024: cfg.install_1024_icons,
            require_sums: cfg.require_sha256sums,
            check_interval: cfg.check_interval_minutes,
            keep_versions: cfg.keep_versions,
            parallel_checks: cfg.parallel_checks,
            network_retries: cfg.network_retries,
            follow_omarchy_theme: cfg.follow_omarchy_theme,
            require_attestations: cfg.require_attestations,
            require_signed_manifest: cfg.require_signed_manifest,
            require_gpg_signature: cfg.require_gpg_signature,
            gpg_keyring: cfg.gpg_keyring.clone(),
            gpg_trusted_fingerprints: cfg.gpg_trusted_fingerprints.join(", "),
        }
    }

    fn apply(&self, cfg: &mut Config) {
        cfg.virustotal_api_key = self.vt_key.trim().to_string();
        cfg.github_token = self.github_token.trim().to_string();
        cfg.vt.allow_unknown = self.allow_unknown;
        cfg.install_1024_icons = self.install_1024;
        cfg.require_sha256sums = self.require_sums;
        cfg.check_interval_minutes = self.check_interval.clamp(5, 24 * 60);
        cfg.keep_versions = self.keep_versions;
        cfg.parallel_checks = self.parallel_checks;
        cfg.network_retries = self.network_retries;
        cfg.follow_omarchy_theme = self.follow_omarchy_theme;
        cfg.require_attestations = self.require_attestations;
        cfg.require_signed_manifest = self.require_signed_manifest;
        cfg.require_gpg_signature = self.require_gpg_signature;
        cfg.gpg_keyring = self.gpg_keyring.trim().to_string();
        cfg.gpg_trusted_fingerprints = self
            .gpg_trusted_fingerprints
            .split([',', '\n', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
}

struct HubApp {
    paths: Paths,
    cfg: Config,
    statuses: Vec<AppStatus>,
    selected: Option<String>,
    view: View,
    busy: Option<Busy>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    icons_started: bool,
    log: Vec<String>,
    download: Option<(u64, Option<u64>)>,
    search: String,
    show_updates: bool,
    auto_opened_updates: bool,
    update_selection: HashMap<String, bool>,
    settings: SettingsDraft,
    toast: Option<(String, bool, Instant)>,
    confirm_uninstall: Option<String>,
    show_log: bool,
    vt_cache: VtCache,
    filter: Filter,
    focus_search: bool,
}

impl HubApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let paths = Paths::from_env();
        let _ = paths.ensure();
        let cfg = Config::load(&paths).unwrap_or_default();
        let theme = if cfg.follow_omarchy_theme {
            Theme::load(&paths)
        } else {
            Theme::default()
        };
        set_palette(&theme);
        let (tx, rx) = channel();

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = bg();
        visuals.window_fill = card();
        visuals.extreme_bg_color = Color32::from_rgb(10, 13, 19);
        visuals.faint_bg_color = card();
        visuals.override_text_color = Some(text());
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border());
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(30, 37, 50);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(30, 37, 50);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(40, 49, 65);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(40, 49, 65);
        visuals.widgets.active.bg_fill = Color32::from_rgb(48, 58, 76);
        visuals.selection.bg_fill = accent().linear_multiply(0.35);
        visuals.selection.stroke = Stroke::new(1.0, accent());
        cc.egui_ctx.set_visuals(visuals);

        cc.egui_ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = Vec2::new(8.0, 8.0);
            style.spacing.button_padding = Vec2::new(12.0, 7.0);
            style.text_styles.insert(
                egui::TextStyle::Heading,
                FontId::new(22.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Body,
                FontId::new(14.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Button,
                FontId::new(14.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Small,
                FontId::new(11.5, egui::FontFamily::Proportional),
            );
        });

        Self {
            settings: SettingsDraft::from_config(&cfg),
            vt_cache: VtCache::load(&paths),
            paths,
            cfg,
            statuses: Vec::new(),
            selected: None,
            view: View::Apps,
            busy: None,
            tx,
            rx,
            icons_started: false,
            log: Vec::new(),
            download: None,
            search: String::new(),
            show_updates: false,
            auto_opened_updates: false,
            update_selection: HashMap::new(),
            toast: None,
            confirm_uninstall: None,
            show_log: false,
            filter: Filter::default(),
            focus_search: false,
        }
    }

    // -----------------------------------------------------------------------
    // Background work
    // -----------------------------------------------------------------------

    fn spawn_check(&mut self, ctx: &egui::Context, force: bool) {
        if self.busy.is_some() {
            self.toast("Please wait, a task is already running.", false);
            return;
        }
        self.busy = Some(Busy::Check);
        self.download = None;
        let tx = self.tx.clone();
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = |event: Progress| {
                let _ = tx.send(Msg::Progress(event));
                ctx.request_repaint();
            };
            let statuses = ops::check_all(&paths, &cfg, force, &report);
            let _ = tx.send(Msg::Status(statuses));
            ctx.request_repaint();
        });
    }

    fn spawn_install(&mut self, ctx: &egui::Context, id: String) {
        if self.busy.is_some() {
            return;
        }
        let name = catalog::by_id(&id)
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| id.clone());
        self.busy = Some(Busy::Install(name.clone()));
        self.download = None;
        self.log.clear();

        let tx = self.tx.clone();
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = |event: Progress| {
                let _ = tx.send(Msg::Progress(event));
                ctx.request_repaint();
            };
            let (text, ok) = match ops::install(&id, &paths, &cfg, false, &report) {
                Ok(outcome) => {
                    let mut text = format!(
                        "{} {} installed successfully.",
                        name,
                        outcome.version
                    );
                    if let Some(vt) = &outcome.vt {
                        text.push_str(&format!(" VirusTotal: {}.", vt.verdict.label()));
                    } else if outcome.already_installed {
                        text = format!("{} {} is already installed.", name, outcome.version);
                    }
                    (text, true)
                }
                Err(error) => (format!("{name}: {error}"), false),
            };
            let _ = tx.send(Msg::Done { text, ok });
            ctx.request_repaint();
        });
    }

    fn spawn_upgrade(&mut self, ctx: &egui::Context, ids: Vec<String>) {
        if self.busy.is_some() || ids.is_empty() {
            return;
        }
        self.busy = Some(Busy::Upgrade);
        self.download = None;
        self.log.clear();

        let tx = self.tx.clone();
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = |event: Progress| {
                let _ = tx.send(Msg::Progress(event));
                ctx.request_repaint();
            };
            let total = ids.len();
            let mut succeeded = 0usize;
            let mut failures = Vec::new();
            for id in ids {
                let name = catalog::by_id(&id)
                    .map(|a| a.name.to_string())
                    .unwrap_or_else(|| id.clone());
                match ops::install(&id, &paths, &cfg, false, &report) {
                    Ok(_) => succeeded += 1,
                    Err(error) => failures.push(format!("{name}: {error}")),
                }
            }
            let mut text = format!("{succeeded} of {total} app(s) updated.");
            if !failures.is_empty() {
                text.push_str(" Failed: ");
                text.push_str(&failures.join(" · "));
            }
            let _ = tx.send(Msg::Done {
                text,
                ok: failures.is_empty(),
            });
            ctx.request_repaint();
        });
    }

    fn spawn_install_all(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        let ids: Vec<String> = self
            .statuses
            .iter()
            .filter(|s| s.installed_version.is_none() && s.plan.is_some())
            .map(|s| s.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        self.busy = Some(Busy::InstallAll);
        self.download = None;
        self.log.clear();

        let tx = self.tx.clone();
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = |event: Progress| {
                let _ = tx.send(Msg::Progress(event));
                ctx.request_repaint();
            };
            let total = ids.len();
            let mut succeeded = 0usize;
            let mut failures = Vec::new();
            for id in ids {
                let name = catalog::by_id(&id)
                    .map(|a| a.name.to_string())
                    .unwrap_or_else(|| id.clone());
                match ops::install(&id, &paths, &cfg, false, &report) {
                    Ok(_) => succeeded += 1,
                    Err(error) => failures.push(format!("{name}: {error}")),
                }
            }
            let mut text = format!("{succeeded} of {total} app(s) installed.");
            if !failures.is_empty() {
                text.push_str(" Failed: ");
                text.push_str(&failures.join(" · "));
            }
            let _ = tx.send(Msg::Done {
                text,
                ok: failures.is_empty(),
            });
            ctx.request_repaint();
        });
    }

    fn spawn_vt_recheck(&mut self, ctx: &egui::Context, id: String) {
        if self.busy.is_some() {
            return;
        }
        let name = catalog::by_id(&id)
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| id.clone());
        self.busy = Some(Busy::Check);
        self.download = None;
        self.log.clear();

        let tx = self.tx.clone();
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = |event: Progress| {
                let _ = tx.send(Msg::Progress(event));
                ctx.request_repaint();
            };
            match ops::vt_recheck(&id, &paths, &cfg, &report) {
                Ok(vt) => {
                    let text = format!(
                        "VirusTotal re-check for {name}: {} ({} of {} engines).",
                        vt.verdict.label(),
                        vt.stats.bad(),
                        vt.stats.total()
                    );
                    let _ = tx.send(Msg::Done { text, ok: vt.verdict != artcraft_core::vt::Verdict::Blocked });
                }
                Err(error) => {
                    let _ = tx.send(Msg::Done {
                        text: format!("{name}: {error}"),
                        ok: false,
                    });
                }
            }
            ctx.request_repaint();
        });
    }

    // -----------------------------------------------------------------------
    // Message handling
    // -----------------------------------------------------------------------

    fn drain(&mut self, ctx: &egui::Context) {
        let mut needs_check = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(statuses) => {
                    self.busy = None;
                    self.download = None;
                    self.vt_cache = VtCache::load(&self.paths);
                    let errors = statuses.iter().filter(|s| s.error.is_some()).count();
                    self.statuses = statuses;
                    if self.selected.is_none() {
                        self.selected = self.statuses.first().map(|s| s.id.clone());
                    }
                    for status in &self.statuses {
                        if status.update_available {
                            self.update_selection
                                .entry(status.id.clone())
                                .or_insert(true);
                        }
                    }
                    if errors > 0 {
                        self.push_log(format!(
                            "{} app(s) could not be checked.",
                            errors
                        ));
                    } else {
                        self.push_log("Release check complete.".to_string());
                    }
                    if !self.auto_opened_updates
                        && self.statuses.iter().any(|s| s.update_available)
                    {
                        self.show_updates = true;
                        self.auto_opened_updates = true;
                    }
                }
                Msg::Progress(Progress::Message(text)) => self.push_log(text),
                Msg::Progress(Progress::Download { received, total }) => {
                    self.download = Some((received, total));
                }
                Msg::Done { text, ok } => {
                    self.busy = None;
                    self.download = None;
                    self.vt_cache = VtCache::load(&self.paths);
                    self.push_log(text.clone());
                    self.toast(text, ok);
                    needs_check = true;
                }
                // (icons are loaded synchronously from embedded data)
            }
        }
        if needs_check {
            self.spawn_check(ctx, false);
        }
    }

    fn push_log(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.log.push(text);
        if self.log.len() > 400 {
            self.log.drain(..self.log.len() - 400);
        }
    }

    fn toast(&mut self, text: impl Into<String>, ok: bool) {
        self.toast = Some((text.into(), ok, Instant::now()));
    }

    // -----------------------------------------------------------------------
    // UI
    // -----------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(panel())
                    .inner_margin(Margin::symmetric(18, 12))
                    .stroke(Stroke::new(1.0, border())),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("ArtCraft")
                            .size(24.0)
                            .strong()
                            .color(text()),
                    );
                    ui.label(
                        RichText::new("HUB")
                            .size(20.0)
                            .strong()
                            .color(accent()),
                    );
                    ui.add_space(24.0);
                    if ui
                        .selectable_label(self.view == View::Apps, "Apps")
                        .clicked()
                    {
                        self.view = View::Apps;
                    }
                    if ui
                        .selectable_label(self.view == View::Settings, "Settings")
                        .clicked()
                    {
                        self.view = View::Settings;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(busy) = &self.busy {
                            ui.add(egui::Spinner::new().size(16.0).color(accent()));
                            ui.label(RichText::new(busy.label()).color(muted()));
                        } else {
                            let updates = self
                                .statuses
                                .iter()
                                .filter(|s| s.update_available)
                                .count();
                            if updates > 0
                                && ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(format!("{updates} Update(s)"))
                                                .color(Color32::BLACK)
                                                .strong(),
                                        )
                                        .fill(accent())
                                        .corner_radius(CornerRadius::same(8)),
                                    )
                                    .clicked()
                            {
                                self.show_updates = true;
                            }
                            let not_installed = self
                                .statuses
                                .iter()
                                .filter(|s| s.installed_version.is_none() && s.plan.is_some())
                                .count();
                            if not_installed > 0
                                && ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(format!("Install all ({not_installed})"))
                                                .color(Color32::BLACK)
                                                .strong(),
                                        )
                                        .fill(accent())
                                        .corner_radius(CornerRadius::same(8)),
                                    )
                                    .clicked()
                            {
                                self.spawn_install_all(&ctx);
                            }
                            if ui.button("Refresh").clicked() {
                                self.spawn_check(&ctx, true);
                            }
                        }
                    });
                });
            });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("bottom")
            .frame(
                egui::Frame::new()
                    .fill(panel())
                    .inner_margin(Margin::symmetric(18, 8))
                    .stroke(Stroke::new(1.0, border())),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.busy.is_some() {
                        if let Some((received, total)) = self.download {
                            let fraction = total
                                .filter(|t| *t > 0)
                                .map(|t| received as f32 / t as f32)
                                .unwrap_or(0.0);
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_width(220.0)
                                    .corner_radius(CornerRadius::same(4))
                                    .fill(accent()),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{:.1} / {:.1} MB",
                                    received as f64 / 1_048_576.0,
                                    total.unwrap_or(0) as f64 / 1_048_576.0
                                ))
                                .small()
                                .color(muted()),
                            );
                        } else {
                            ui.label(RichText::new("Working …").color(muted()));
                        }
                    } else if let Some(last) = self.log.last() {
                        ui.label(
                            RichText::new(truncate(last, 110))
                                .small()
                                .color(muted()),
                        );
                    } else {
                        ui.label(RichText::new("Ready.").small().color(muted()));
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .small()
                                .color(muted()),
                        );
                        if ui
                            .selectable_label(
                                self.show_log,
                                RichText::new("Log").small().color(text()),
                            )
                            .clicked()
                        {
                            self.show_log = !self.show_log;
                        }
                        let vt = if self.cfg.vt_key().is_some() {
                            RichText::new("VirusTotal active").small().color(ok())
                        } else {
                            RichText::new("VirusTotal key missing").small().color(bad())
                        };
                        ui.label(vt);
                    });
                });
            });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("apps")
            .exact_size(330.0)
            .frame(
                egui::Frame::new()
                    .fill(panel())
                    .inner_margin(Margin::symmetric(12, 12))
                    .stroke(Stroke::new(1.0, border())),
            )
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for filter in [Filter::All, Filter::Updates, Filter::Installed, Filter::NotInstalled] {
                        if ui
                            .selectable_label(self.filter == filter, filter.label())
                            .clicked()
                        {
                            self.filter = filter;
                        }
                    }
                });
                ui.add_space(4.0);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .id_source("search")
                        .hint_text("Search apps …")
                        .desired_width(f32::INFINITY),
                );
                if self.focus_search {
                    response.request_focus();
                    self.focus_search = false;
                }
                ui.add_space(4.0);

                let search = self.search.trim().to_lowercase();
                let mut clicked = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for status in &self.statuses {
                            if !matches_filter(status, self.filter) {
                                continue;
                            }
                            if !search.is_empty()
                                && !status.name.to_lowercase().contains(&search)
                                && !status.id.contains(&search)
                            {
                                continue;
                            }
                            let selected = self.selected.as_deref() == Some(&status.id);
                            let frame = egui::Frame::new()
                                .fill(if selected { card_hover() } else { card() })
                                .corner_radius(CornerRadius::same(10))
                                .inner_margin(Margin::symmetric(10, 8))
                                .stroke(Stroke::new(
                                    1.0,
                                    if selected { accent() } else { border() },
                                ));
                            let row = frame
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        self.icon(ui, status, 38.0);
                                        ui.add_space(4.0);
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(&status.name)
                                                    .strong()
                                                    .color(text()),
                                            );
                                            let (text, color) = side_status(status);
                                            ui.label(
                                                RichText::new(truncate(&text, 30))
                                                    .small()
                                                    .color(color),
                                            );
                                        });
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if status.update_available {
                                                    update_glyph(ui);
                                                } else if status.installed_version.is_some()
                                                {
                                                    status_glyph(ui, true);
                                                }
                                            },
                                        );
                                    });
                                })
                                .response;
                            if row.interact(Sense::click()).clicked() {
                                clicked = Some(status.id.clone());
                            }
                            ui.add_space(6.0);
                        }
                    });
                if let Some(id) = clicked {
                    self.selected = Some(id);
                    self.view = View::Apps;
                }
            });
    }

    fn icon(&self, ui: &mut egui::Ui, status: &AppStatus, size: f32) {
        // Load from a process-global cache so textures survive any reset.
        static ICON_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, egui::TextureHandle>>> =
            std::sync::Mutex::new(None);
        let texture = {
            let mut cache = ICON_CACHE.lock().unwrap();
            if cache.is_none() {
                *cache = Some(std::collections::HashMap::new());
            }
            let cache = cache.as_mut().unwrap();
            if !cache.contains_key(&status.id) {
                // Prefer the embedded, padding-free icons (always available).
                let img = if let Some(bytes) = embedded_icon(&status.id) {
                    image::load_from_memory(bytes).ok().map(|img| to_color_image(&img))
                } else {
                    let bundle = catalog::by_id(&status.id).map(|a| a.bundle_id()).unwrap_or_default();
                    local_icon(&self.paths, &status.id, &bundle).and_then(|p| load_image(&p))
                };
                if let Some(image) = img {
                    let tex = ui.ctx().load_texture(
                        format!("icon-{}", status.id),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    cache.insert(status.id.clone(), tex);
                }
            }
            cache.get(&status.id).cloned()
        };
        if let Some(texture) = texture {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
            ui.painter().image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            let (response, painter) =
                ui.allocate_painter(Vec2::splat(size), Sense::hover());
            let rect = response.rect;
            let color = Color32::from_rgb(
                status.color[0],
                status.color[1],
                status.color[2],
            );
            painter.circle_filled(rect.center(), size / 2.0, color.linear_multiply(0.85));
            let initial = status.name.chars().next().unwrap_or('?');
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                initial,
                FontId::proportional(size * 0.42),
                Color32::WHITE,
            );
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(bg())
                    .inner_margin(Margin::symmetric(24, 18)),
            )
            .show(ui, |ui| {
                if self.view == View::Settings {
                    self.settings_view(ui);
                    return;
                }
                if self.cfg.vt_key().is_none() {
                    egui::Frame::new()
                        .fill(Color32::from_rgb(58, 30, 34))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(14, 12))
                        .stroke(Stroke::new(1.0, bad().linear_multiply(0.6)))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("⚠").color(bad()).strong());
                                ui.label(
                                    RichText::new(
                                        "No VirusTotal API key configured. Installs are locked until a key is set.",
                                    )
                                    .color(text()),
                                );
                                if ui.button("Set up now").clicked() {
                                    self.view = View::Settings;
                                }
                            });
                        });
                    ui.add_space(10.0);
                }

                let Some(status) = self
                    .statuses
                    .iter()
                    .find(|s| Some(&s.id) == self.selected.as_ref())
                    .cloned()
                else {
                    ui.centered_and_justified(|ui| {
                        if self.busy.is_some() {
                            ui.add(egui::Spinner::new().size(28.0).color(accent()));
                        } else {
                            ui.label(RichText::new("No app selected.").color(muted()));
                        }
                    });
                    return;
                };

                let app = catalog::by_id(&status.id);
                ui.horizontal(|ui| {
                    self.icon(ui, &status, 92.0);
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&status.name)
                                .size(30.0)
                                .strong()
                                .color(text()),
                        );
                        ui.label(
                            RichText::new(&status.tagline)
                                .size(15.0)
                                .color(muted()),
                        );
                        if let Some(app) = app {
                            ui.hyperlink_to(
                                RichText::new("getartcraft.com").small().color(accent()),
                                app.website(),
                            );
                        }
                    });
                });

                ui.add_space(14.0);
                self.action_row(ui, &status, &ctx);
                ui.add_space(14.0);

                self.info_card(ui, &status);
                ui.add_space(12.0);
                self.security_card(ui, &status);
                ui.add_space(12.0);

                egui::Frame::new()
                    .fill(card())
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 12))
                    .stroke(Stroke::new(1.0, border()))
                    .show(ui, |ui| {
                        ui.label(RichText::new("Release notes").strong().color(text()));
                        ui.add_space(4.0);
                        let notes = status
                            .plan
                            .as_ref()
                            .map(|p| p.notes.trim())
                            .filter(|n| !n.is_empty())
                            .unwrap_or("No release notes available.");
                        egui::ScrollArea::vertical()
                            .id_salt("notes")
                            .max_height(200.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(truncate(notes, 6000))
                                        .monospace()
                                        .size(12.0)
                                        .color(muted()),
                                );
                            });
                    });
            });
    }

    fn action_row(&mut self, ui: &mut egui::Ui, status: &AppStatus, ctx: &egui::Context) {
        let busy = self.busy.is_some();
        ui.horizontal_wrapped(|ui| {
            let install_label = if status.installed_version.is_none() {
                "Install"
            } else if status.update_available {
                "Update"
            } else {
                "Reinstall"
            };
            let enabled = !busy && status.plan.is_some() && self.cfg.vt_key().is_some();
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(
                        RichText::new(install_label).strong().color(Color32::BLACK),
                    )
                    .fill(accent())
                    .corner_radius(CornerRadius::same(8))
                    .min_size(Vec2::new(140.0, 34.0)),
                )
                .clicked()
            {
                self.spawn_install(ctx, status.id.clone());
            }

            if status.installed_version.is_some()
                && ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(RichText::new("Open").color(text()))
                            .corner_radius(CornerRadius::same(8))
                            .min_size(Vec2::new(90.0, 34.0)),
                    )
                    .clicked()
            {
                match ops::open_app(&status.id, &self.paths) {
                    Ok(()) => self.toast(format!("Opened {}.", status.name), true),
                    Err(error) => self.toast(format!("Could not open {}: {error}", status.name), false),
                }
            }

            if status.installed_version.is_some()
                && ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(RichText::new("Remove").color(bad()))
                            .corner_radius(CornerRadius::same(8))
                            .min_size(Vec2::new(120.0, 34.0)),
                    )
                    .clicked()
            {
                self.confirm_uninstall = Some(status.id.clone());
            }

            if status.installed_version.is_some()
                && ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(
                            RichText::new(if status.pinned { "Unpin" } else { "Pin" }).color(text()),
                        )
                        .corner_radius(CornerRadius::same(8))
                        .min_size(Vec2::new(90.0, 34.0)),
                    )
                    .clicked()
            {
                let id = status.id.clone();
                let ctx = ctx.clone();
                if status.pinned {
                    match ops::unpin(&id, &self.paths) {
                        Ok(_) => self.toast(format!("{} can be updated again.", status.name), true),
                        Err(error) => self.toast(format!("Unpin failed: {error}"), false),
                    }
                } else {
                    match ops::pin(&id, None, &self.paths) {
                        Ok(tag) => self.toast(format!("{} pinned to {tag}.", status.name), true),
                        Err(error) => self.toast(format!("Pin failed: {error}"), false),
                    }
                }
                self.spawn_check(&ctx, false);
            }

            if status.installed_version.is_some()
                && status.rollback_version.is_some()
                && ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(RichText::new(format!(
                            "Roll back to {}",
                            status.rollback_version.as_deref().unwrap_or("?")
                        ))
                        .color(text()))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(Vec2::new(140.0, 34.0)),
                    )
                    .clicked()
            {
                let tx = self.tx.clone();
                let paths = self.paths.clone();
                let busy_name = status.name.clone();
                let name = status.name.clone();
                let id = status.id.clone();
                let target = status.rollback_version.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let progress = |event: Progress| {
                        let _ = tx.send(Msg::Progress(event));
                        ctx.request_repaint();
                    };
                    match ops::rollback(&id, target.as_deref(), &paths, &progress) {
                        Ok(outcome) => {
                            let _ = tx.send(Msg::Done {
                                text: format!("{} {} is active and pinned.", name.clone(), outcome.version),
                                ok: true,
                            });
                        }
                        Err(error) => {
                            let _ = tx.send(Msg::Done {
                                text: format!("{name}: {error}"),
                                ok: false,
                            });
                        }
                    }
                    ctx.request_repaint();
                });
                self.busy = Some(Busy::Install(busy_name));
            }

            if status.update_available || status.skipped {
                let label = if status.skipped {
                    "Don't skip"
                } else {
                    "Skip this version"
                };
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(RichText::new(label).color(text()))
                            .corner_radius(CornerRadius::same(8))
                            .min_size(Vec2::new(130.0, 34.0)),
                    )
                    .clicked()
                {
                    let id = status.id.clone();
                    let ctx = ctx.clone();
                    if status.skipped {
                        match ops::unskip(&id, &self.paths) {
                            Ok(_) => self.toast("Version is no longer skipped.", true),
                            Err(error) => self.toast(format!("Unskip failed: {error}"), false),
                        }
                    } else if let Some(tag) = &status.latest_tag {
                        match ops::skip(&id, Some(tag), &self.paths) {
                            Ok(_) => self.toast(format!("{tag} skipped."), true),
                            Err(error) => self.toast(format!("Skip failed: {error}"), false),
                        }
                    }
                    self.spawn_check(&ctx, false);
                }
            }

            if status.plan.is_some() && self.cfg.vt_key().is_none() {
                ui.label(
                    RichText::new("VirusTotal key required")
                        .small()
                        .color(warn()),
                );
            }
        });
        if let Some(error) = &status.error {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Note: {error}")).small().color(warn()));
        }
    }

    fn info_card(&self, ui: &mut egui::Ui, status: &AppStatus) {
        egui::Frame::new()
            .fill(card())
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(14, 12))
            .stroke(Stroke::new(1.0, border()))
            .show(ui, |ui| {
                egui::Grid::new("info")
                    .num_columns(2)
                    .spacing([24.0, 7.0])
                    .show(ui, |ui| {
                        row(ui, "Installed", status.installed_version.as_deref().unwrap_or("–"), text());
                        row(
                            ui,
                            "Latest version",
                            status.latest_version.as_deref().unwrap_or("–"),
                            if status.update_available { warn() } else { text() },
                        );
                        row(
                            ui,
                            "Release",
                            status.latest_tag.as_deref().unwrap_or("–"),
                            muted(),
                        );
                        if status.pinned {
                            row(
                                ui,
                                "Pin",
                                status.pinned_tag.as_deref().unwrap_or("pinned"),
                                accent(),
                            );
                        }
                        if status.skipped {
                            row(ui, "Skipped", "latest release skipped", warn());
                        }
                        if let Some(version) = &status.rollback_version {
                            row(ui, "Rollback", version, muted());
                        }
                        if !status.versions.is_empty() {
                            row(
                                ui,
                                "Versions",
                                &status.versions.join(", "),
                                muted(),
                            );
                        }
                        let sha = status
                            .latest_sha256
                            .as_deref()
                            .map(|s| format!("{}…", &s[..16.min(s.len())]))
                            .unwrap_or_else(|| "–".into());
                        ui.label(RichText::new("SHA-256").color(muted()));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(sha).monospace().color(text()));
                            if let Some(sha) = &status.latest_sha256 {
                                if ui.small_button("copy").clicked() {
                                    ui.ctx().copy_text(sha.clone());
                                }
                            }
                        });
                        ui.end_row();
                    });
            });
    }

    fn security_card(&mut self, ui: &mut egui::Ui, status: &AppStatus) {
        egui::Frame::new()
            .fill(card())
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(14, 12))
            .stroke(Stroke::new(1.0, border()))
            .show(ui, |ui| {
                ui.label(RichText::new("Security check").strong().color(text()));
                ui.add_space(2.0);
                check_line(
                    ui,
                    true,
                    "SHA-256 against the release SHA256SUMS.txt (GitHub digest as cross-check)",
                );
                check_line(
                    ui,
                    self.cfg.vt_key().is_some(),
                    if self.cfg.vt_key().is_some() {
                        "VirusTotal: file report and URL scan, content-hash verification"
                    } else {
                        "VirusTotal: no API key configured – installs locked"
                    },
                );
                check_line(
                    ui,
                    true,
                    "No root; installs only into ~/.local",
                );
                if self.cfg.vt_key().is_some() {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if let Some(sha) = &status.latest_sha256 {
                            let url = self
                                .vt_cache
                                .entries
                                .get(&sha.to_ascii_lowercase())
                                .map(|result| {
                                    result.permalink_with(
                                        status.plan.as_ref().map(|plan| plan.asset_url.as_str()),
                                    )
                                })
                                .unwrap_or_else(|| vt::file_report_permalink(sha));
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("VirusTotal report").small().color(accent()),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                open_url(&url);
                            }
                        }
                        let busy = self.busy.is_some();
                        let ctx = ui.ctx().clone();
                        let id = status.id.clone();
                        if ui
                            .add_enabled(!busy, egui::Button::new("Re-check VirusTotal").small())
                            .clicked()
                        {
                            self.spawn_vt_recheck(&ctx, id);
                        }
                    });
                }
            });
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.label(
            RichText::new(
                "Configuration is stored in the user config directory.",
            )
            .small()
            .color(muted()),
        );
        ui.add_space(8.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.columns(2, |cols| {
                    cols[0].vertical(|ui| {
                        egui::Frame::new()
                            .fill(card())
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 10))
                            .stroke(Stroke::new(1.0, border()))
                            .show(ui, |ui| {
                                ui.label(RichText::new("VirusTotal").strong().color(text()));
                                ui.label(RichText::new("Public API: 4 requests per minute, 500 per day.").small().color(muted()));
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(&mut self.settings.vt_key).password(true).hint_text("VirusTotal API key").desired_width(260.0));
                                    if ui.button("Create key").clicked() { open_url("https://www.virustotal.com/gui/my-apikey"); }
                                });
                            });
                        ui.add_space(8.0);
                        egui::Frame::new()
                            .fill(card())
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 10))
                            .stroke(Stroke::new(1.0, border()))
                            .show(ui, |ui| {
                                ui.label(RichText::new("GitHub").strong().color(text()));
                                ui.label(RichText::new("Optional. Raises the API limit from 60 to 5,000 req/h.").small().color(muted()));
                                ui.add_space(4.0);
                                ui.add(egui::TextEdit::singleline(&mut self.settings.github_token).password(true).hint_text("GitHub token (optional)").desired_width(300.0));
                            });
                    });
                    cols[1].vertical(|ui| {
                        egui::Frame::new()
                            .fill(card())
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 10))
                            .stroke(Stroke::new(1.0, border()))
                            .show(ui, |ui| {
                                ui.label(RichText::new("Behaviour").strong().color(text()));
                                ui.checkbox(&mut self.settings.require_sums, "Require SHA256SUMS.txt");
                                ui.checkbox(&mut self.settings.install_1024, "Install 1024 px icon");
                                ui.checkbox(&mut self.settings.follow_omarchy_theme, "Follow theme");
                                ui.horizontal(|ui| { ui.label("Check interval:"); ui.add(egui::DragValue::new(&mut self.settings.check_interval).range(5..=1440).suffix(" min")); });
                                ui.horizontal(|ui| { ui.label("Versions kept:"); ui.add(egui::DragValue::new(&mut self.settings.keep_versions).range(1..=10)); });
                                ui.horizontal(|ui| { ui.label("Parallel checks:"); ui.add(egui::DragValue::new(&mut self.settings.parallel_checks).range(1..=8)); });
                                ui.horizontal(|ui| { ui.label("Network attempts:"); ui.add(egui::DragValue::new(&mut self.settings.network_retries).range(1..=6)); });
                            });
                        ui.add_space(8.0);
                        egui::Frame::new()
                            .fill(Color32::from_rgb(46, 35, 20))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 10))
                            .stroke(Stroke::new(1.0, warn().linear_multiply(0.5)))
                            .show(ui, |ui| {
                                ui.label(RichText::new("Advanced").strong().color(warn()));
                                ui.checkbox(&mut self.settings.allow_unknown, "Allow unclear VirusTotal results");
                                ui.label(RichText::new("Not recommended.").small().color(muted()));
                                ui.add_space(4.0);
                                ui.label(RichText::new("Trust anchors").strong().color(text()));
                                ui.checkbox(&mut self.settings.require_attestations, "Require attestations");
                                ui.checkbox(&mut self.settings.require_signed_manifest, "Require signed manifest");
                                ui.checkbox(&mut self.settings.require_gpg_signature, "Require GPG signature");
                                ui.add(egui::TextEdit::singleline(&mut self.settings.gpg_keyring).hint_text("GPG keyring path (empty = default)").desired_width(260.0));
                                ui.add(egui::TextEdit::singleline(&mut self.settings.gpg_trusted_fingerprints).hint_text("Trusted GPG fingerprints").desired_width(260.0));
                            });
                    });
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Save").strong().color(Color32::BLACK)).fill(accent()).corner_radius(CornerRadius::same(8)).min_size(Vec2::new(130.0, 34.0))).clicked() {
                        self.settings.apply(&mut self.cfg);
                        match self.cfg.save(&self.paths) {
                            Ok(()) => { self.toast("Settings saved.", true); self.spawn_check(ui.ctx(), false); }
                            Err(error) => self.toast(format!("Saving failed: {error}"), false),
                        }
                    }
                    if ui.button("Open config folder").clicked() {
                        open_url(&format!("file://{}", self.paths.config_dir.display()));
                    }
                    if ui.button("Reset").clicked() {
                        self.settings = SettingsDraft::from_config(&self.cfg);
                    }
                });
            });
    }

    fn update_window(&mut self, ctx: &egui::Context) {
        if !self.show_updates {
            return;
        }
        let updates: Vec<AppStatus> = self
            .statuses
            .iter()
            .filter(|s| s.update_available)
            .cloned()
            .collect();
        if updates.is_empty() {
            self.show_updates = false;
            return;
        }
        let mut open = true;
        egui::Window::new("Updates available")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(520.0)
            .open(&mut open)
            .frame(
                egui::Frame::new()
                    .fill(panel())
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18))
                    .stroke(Stroke::new(1.0, border())),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} app(s) can be updated.",
                        updates.len()
                    ))
                    .color(text()),
                );
                ui.label(
                    RichText::new("Every update is re-checked via SHA-256 and VirusTotal.")
                        .small()
                        .color(muted()),
                );
                ui.add_space(8.0);
                for status in &updates {
                    let mut checked = self
                        .update_selection
                        .get(&status.id)
                        .copied()
                        .unwrap_or(true);
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut checked, "");
                        self.icon(ui, status, 28.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&status.name).strong());
                            ui.label(
                                RichText::new(gui_text(ops::version_label(
                                    status.installed_version.as_deref(),
                                    status.latest_version.as_deref(),
                                )))
                                .small()
                                .color(muted()),
                            );
                        });
                    });
                    self.update_selection.insert(status.id.clone(), checked);
                    ui.add_space(2.0);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Select all").clicked() {
                        for status in &updates {
                            self.update_selection.insert(status.id.clone(), true);
                        }
                    }
                    if ui.button("None").clicked() {
                        for status in &updates {
                            self.update_selection.insert(status.id.clone(), false);
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let selected: Vec<String> = updates
                            .iter()
                            .filter(|s| {
                                self.update_selection
                                    .get(&s.id)
                                    .copied()
                                    .unwrap_or(true)
                            })
                            .map(|s| s.id.clone())
                            .collect();
                        let enabled = !selected.is_empty() && self.busy.is_none();
                        if ui
                            .add_enabled(
                                enabled,
                                egui::Button::new(
                                    RichText::new(format!(
                                        "Update now ({})",
                                        selected.len()
                                    ))
                                    .strong()
                                    .color(Color32::BLACK),
                                )
                                .fill(accent())
                                .corner_radius(CornerRadius::same(8)),
                            )
                            .clicked()
                        {
                            self.show_updates = false;
                            self.spawn_upgrade(ctx, selected);
                        }
                        if ui.button("Later").clicked() {
                            self.show_updates = false;
                        }
                    });
                });
            });
        if !open {
            self.show_updates = false;
        }
    }

    fn uninstall_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_uninstall.clone() else {
            return;
        };
        let name = catalog::by_id(&id)
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| id.clone());
        let mut open = true;
        egui::Window::new("Remove app")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{name} will be removed from ~/.local/opt/artcraft. App settings are kept."
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Remove").color(Color32::WHITE))
                                .fill(bad().linear_multiply(0.8))
                                .corner_radius(CornerRadius::same(8)),
                        )
                        .clicked()
                    {
                        match ops::uninstall(&id, &self.paths) {
                            Ok(removed) => {
                                self.toast(
                                    if removed {
                                        format!("{name} was removed.")
                                    } else {
                                        format!("{name} was not installed.")
                                    },
                                    true,
                                );
                                self.spawn_check(ctx, false);
                            }
                            Err(error) => {
                                self.toast(format!("Removal failed: {error}"), false)
                            }
                        }
                        self.confirm_uninstall = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_uninstall = None;
                    }
                });
            });
        if !open {
            self.confirm_uninstall = None;
        }
    }

    fn log_window(&mut self, ctx: &egui::Context) {
        if !self.show_log {
            return;
        }
        let mut open = true;
        egui::Window::new("Log")
            .open(&mut open)
            .default_width(640.0)
            .default_height(360.0)
            .frame(
                egui::Frame::new()
                    .fill(panel())
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(14))
                    .stroke(Stroke::new(1.0, border())),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(
                                RichText::new(line)
                                    .monospace()
                                    .size(12.0)
                                    .color(muted()),
                            );
                        }
                    });
            });
        self.show_log = open;
    }

    fn toast_layer(&mut self, ctx: &egui::Context) {
        let Some((text, ok, at)) = self.toast.clone() else {
            return;
        };
        if at.elapsed() > Duration::from_secs(8) {
            self.toast = None;
            return;
        }
        egui::Area::new(egui::Id::unique("artcraft-toast"))
            .anchor(Align2::CENTER_BOTTOM, [0.0, -54.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(if ok {
                        Color32::from_rgb(24, 52, 38)
                    } else {
                        Color32::from_rgb(58, 30, 34)
                    })
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(16, 10))
                    .stroke(Stroke::new(
                        1.0,
                        if ok { palette().ok.linear_multiply(0.6) } else { palette().bad.linear_multiply(0.6) },
                    ))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(truncate(&text, 160))
                                .color(if ok { palette().ok } else { palette().bad }),
                        );
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(400));
    }
}

impl eframe::App for HubApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain(&ctx);
        if !self.icons_started {
            self.icons_started = true;
            self.spawn_check(&ctx, false);
        }

        // Keyboard shortcuts.
        let wants_keyboard = ctx.egui_wants_keyboard_input();
        let mut refresh = false;
        let mut toggle_updates = false;
        let mut focus_search = false;
        let mut close_windows = false;
        let mut filter_set: Option<Filter> = None;
        ctx.input(|i| {
            if i.key_pressed(egui::Key::F5)
                || (i.modifiers.ctrl && i.key_pressed(egui::Key::R))
            {
                refresh = true;
            }
            if i.modifiers.ctrl && i.key_pressed(egui::Key::F) {
                focus_search = true;
            }
            if i.modifiers.ctrl && i.key_pressed(egui::Key::U) {
                toggle_updates = true;
            }
            if i.key_pressed(egui::Key::Escape) {
                close_windows = true;
            }
            if !i.modifiers.ctrl && !i.modifiers.alt && !wants_keyboard {
                if i.key_pressed(egui::Key::Num1) {
                    filter_set = Some(Filter::All);
                }
                if i.key_pressed(egui::Key::Num2) {
                    filter_set = Some(Filter::Updates);
                }
                if i.key_pressed(egui::Key::Num3) {
                    filter_set = Some(Filter::Installed);
                }
                if i.key_pressed(egui::Key::Num4) {
                    filter_set = Some(Filter::NotInstalled);
                }
            }
        });
        if refresh {
            self.spawn_check(&ctx, true);
        }
        if focus_search {
            self.focus_search = true;
        }
        if toggle_updates {
            self.show_updates = !self.show_updates;
        }
        if close_windows {
            self.show_updates = false;
            self.show_log = false;
            self.confirm_uninstall = None;
        }
        if let Some(filter) = filter_set {
            self.filter = filter;
        }

        self.top_bar(ui);
        self.bottom_bar(ui);
        self.sidebar(ui);
        self.central(ui);

        self.update_window(&ctx);
        self.uninstall_window(&ctx);
        self.log_window(&ctx);
        self.toast_layer(&ctx);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.label(RichText::new(label).color(muted()));
    ui.label(RichText::new(value).color(color));
    ui.end_row();
}

fn check_line(ui: &mut egui::Ui, ok: bool, text: &str) {
    ui.horizontal_wrapped(|ui| {
        status_glyph(ui, ok);
        ui.label(RichText::new(text).small().color(muted()));
    });
}

/// Small upward arrow for "update available". Drawn with the painter because the
/// bundled egui fonts do not contain U+25CF (●) and render it as a tofu box.
fn update_glyph(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let color = warn();
    let stroke = Stroke::new(2.0, color);
    let c = rect.center();
    painter.line_segment([c + Vec2::new(0.0, 5.0), c + Vec2::new(0.0, -5.0)], stroke);
    painter.line_segment([c + Vec2::new(-4.0, -1.0), c + Vec2::new(0.0, -5.0)], stroke);
    painter.line_segment([c + Vec2::new(4.0, -1.0), c + Vec2::new(0.0, -5.0)], stroke);
}

fn status_glyph(ui: &mut egui::Ui, ok: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let color = if ok { palette().ok } else { palette().bad };
    let c = rect.center();
    if ok {
        painter.line_segment([c + Vec2::new(-5.0, 1.0), c + Vec2::new(-1.5, 5.0)], Stroke::new(2.0, color));
        painter.line_segment([c + Vec2::new(-1.5, 5.0), c + Vec2::new(5.0, -4.0)], Stroke::new(2.0, color));
    } else {
        painter.line_segment([c + Vec2::new(-4.0, -4.0), c + Vec2::new(4.0, 4.0)], Stroke::new(2.0, color));
        painter.line_segment([c + Vec2::new(-4.0, 4.0), c + Vec2::new(4.0, -4.0)], Stroke::new(2.0, color));
    }
}

fn matches_filter(status: &AppStatus, filter: Filter) -> bool {
    match filter {
        Filter::All => true,
        Filter::Updates => status.update_available,
        Filter::Installed => status.installed_version.is_some(),
        Filter::NotInstalled => status.installed_version.is_none(),
    }
}

fn side_status(status: &AppStatus) -> (String, Color32) {
    if status.pinned {
        return (
            format!(
                "Pinned - {}",
                status.pinned_tag.as_deref().unwrap_or("?")
            ),
            accent(),
        );
    }
    if status.skipped {
        return ("Latest release skipped".into(), muted());
    }
    if status.update_available {
        (
            format!(
                "Update {} -> {}",
                status.installed_version.as_deref().unwrap_or("?"),
                status.latest_version.as_deref().unwrap_or("?")
            ),
            warn(),
        )
    } else if let Some(version) = &status.installed_version {
        (format!("Installed - {version}"), ok())
    } else if let Some(error) = &status.error {
        if error.contains("No release") {
            ("No release yet".into(), muted())
        } else {
            ("Check failed".into(), muted())
        }
    } else if let Some(latest) = &status.latest_version {
        (format!("Not installed - {latest}"), muted())
    } else {
        ("Unknown".into(), muted())
    }
}

/// Replaces glyphs that egui's bundled proportional fonts lack. U+2192 (→) is
/// only present in the monospace fallback, so it renders as a box in labels.
fn gui_text(text: String) -> String {
    text.replace('→', "->")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        if url.starts_with("https://") {
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            return;
        }
        if let Some(path) = url.strip_prefix("file://") {
            let _ = std::process::Command::new("explorer")
                .arg(path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            return;
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if url.starts_with("https://") || url.starts_with("file://") {
            let _ = std::process::Command::new("xdg-open")
                .arg(url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

fn load_image(path: &PathBuf) -> Option<egui::ColorImage> {
    let bytes = std::fs::read(path).ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    Some(to_color_image(&image))
}

fn to_color_image(image: &image::DynamicImage) -> egui::ColorImage {
    let image = image.thumbnail(256, 256).to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw())
}

/// Returns the icon bytes embedded in the binary (padding-free WebP, converted
/// to PNG at build time). These are always available, even offline.
fn embedded_icon(id: &str) -> Option<&'static [u8]> {
    match id {
        "photocraft" => Some(include_bytes!("../../../assets/icons/photocraft.png")),
        "vectorcraft" => Some(include_bytes!("../../../assets/icons/vectorcraft.png")),
        "filmcraft" => Some(include_bytes!("../../../assets/icons/filmcraft.png")),
        "lightcraft" => Some(include_bytes!("../../../assets/icons/lightcraft.png")),
        "pdfcraft" => Some(include_bytes!("../../../assets/icons/pdfcraft.png")),
        "effectcraft" => Some(include_bytes!("../../../assets/icons/effectcraft.png")),
        "designcraft" => Some(include_bytes!("../../../assets/icons/designcraft.png")),
        "figcraft" => Some(include_bytes!("../../../assets/icons/figcraft.png")),
        "texturecraft" => Some(include_bytes!("../../../assets/icons/texturecraft.png")),
        "materialcraft" => Some(include_bytes!("../../../assets/icons/materialcraft.png")),
        "wordcraft" => Some(include_bytes!("../../../assets/icons/wordcraft.png")),
        "soundcraft" => Some(include_bytes!("../../../assets/icons/soundcraft.png")),
        "deckcraft" => Some(include_bytes!("../../../assets/icons/deckcraft.png")),
        "cadcraft" => Some(include_bytes!("../../../assets/icons/cadcraft.png")),
        "gridcraft" => Some(include_bytes!("../../../assets/icons/gridcraft.png")),
        _ => None,
    }
}

/// Finds an icon that is already on disk: installed hicolor theme, the app's
/// version directory, or the hub's own cache.
fn local_icon(paths: &Paths, id: &str, bundle: &str) -> Option<PathBuf> {
    // Prefer the padding-free cached WebP (saved as PNG) over the hicolor PNGs.
    let cached = paths.icon_cache_dir().join(format!("{id}.png"));
    if cached.is_file() {
        return Some(cached);
    }
    let sizes = ["1024x1024", "512x512", "256x256", "128x128", "64x64"];
    for size in sizes {
        let candidate = paths
            .icons_dir
            .join("hicolor")
            .join(size)
            .join("apps")
            .join(format!("{bundle}.png"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let root = paths.app_dir(id);
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    versions.reverse();
    for version in versions {
        for size in sizes {
            let candidate = version
                .join("share/icons/hicolor")
                .join(size)
                .join("apps")
                .join(format!("{bundle}.png"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
