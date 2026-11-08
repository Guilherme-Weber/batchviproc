//! Connects the Slint window (ui/app.slint) to the engine.
//!
//! The engine runs on tokio threads and reports through the [`Ui`] trait; updates are
//! collected in an inbox and applied on the UI thread in one batch per event-loop turn.

use crate::args::RawOptions;
use crate::engine::{Engine, StateDto, Status, Ui};
use crate::system;
use crate::taskbar::{Taskbar, TaskbarProgress};

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

slint::include_modules!();

const MAX_LOG_LINES: usize = 500;

const ENCODERS: &[(&str, &str)] = &[
    ("av1_nvenc", "AV1 NVENC (10-bit GPU)"),
    ("hevc_nvenc", "HEVC NVENC (10-bit GPU)"),
    ("h264_nvenc", "H.264 NVENC (GPU)"),
    ("libx264", "x264 (CPU)"),
];
const JOBS: &[(u32, &str)] = &[
    (1, "1 (single stream)"),
    (2, "2 (dual NVENC)"),
    (3, "3 streams"),
    (4, "4 streams"),
];
const QUALITY: &[(u32, &str)] = &[
    (20, "High quality (camera / raw)"),
    (23, "Balanced (web / re-encode)"),
    (26, "Compact (max space saver)"),
];
const SPEEDS: &[(&str, &str)] = &[
    ("slow", "Slow (NVENC p7, best)"),
    ("medium", "Medium (NVENC p7)"),
    ("veryfast", "Very fast (NVENC p5)"),
];

// ------------------------------------------------------------------- settings

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Prefs {
    encoder: String,
    concurrency: u32,
    crf: u32,
    preset: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            encoder: "av1_nvenc".into(),
            concurrency: 2,
            crf: 23,
            preset: "slow".into(),
        }
    }
}

/// Position of `value` in `choices`, falling back to the position of `default`.
fn position<K: PartialEq>(choices: &[(K, &str)], value: &K, default: &K) -> i32 {
    let find = |v: &K| choices.iter().position(|(k, _)| k == v);
    find(value).or_else(|| find(default)).unwrap_or(0) as i32
}

impl Prefs {
    fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Prefs>(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Reads the current selection from the window.
    fn from_window(w: &AppWindow) -> Self {
        let pick = |i: i32, len: usize| (i.max(0) as usize).min(len - 1);
        Self {
            encoder: ENCODERS[pick(w.get_encoder_index(), ENCODERS.len())].0.into(),
            concurrency: JOBS[pick(w.get_jobs_index(), JOBS.len())].0,
            crf: QUALITY[pick(w.get_quality_index(), QUALITY.len())].0,
            preset: SPEEDS[pick(w.get_speed_index(), SPEEDS.len())].0.into(),
        }
    }

    /// Shows the selection in the window; unknown saved values (e.g. the removed OpenCL
    /// encoder) fall back to the defaults.
    fn apply_to_window(&self, w: &AppWindow) {
        let d = Prefs::default();
        w.set_encoder_index(position(ENCODERS, &self.encoder.as_str(), &d.encoder.as_str()));
        w.set_jobs_index(position(JOBS, &self.concurrency, &d.concurrency));
        w.set_quality_index(position(QUALITY, &self.crf, &d.crf));
        w.set_speed_index(position(SPEEDS, &self.preset.as_str(), &d.preset.as_str()));
    }

    fn to_raw(&self) -> RawOptions {
        RawOptions {
            crf: Some(self.crf as f64),
            preset: Some(self.preset.clone()),
            encoder: Some(self.encoder.clone()),
            concurrency: Some(self.concurrency as f64),
        }
    }
}

fn labels<K>(choices: &[(K, &str)]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        choices.iter().map(|(_, l)| SharedString::from(*l)).collect::<Vec<_>>(),
    ))
}

// ------------------------------------------------------------------- engine -> UI bridge

#[derive(Default)]
struct Inbox {
    state: Mutex<Option<StateDto>>,
    logs: Mutex<Vec<String>>,
    scheduled: AtomicBool,
}

struct SlintUi {
    inbox: Arc<Inbox>,
    file_log: crate::logfile::FileLog,
}

impl SlintUi {
    /// Queues one UI-thread update; further messages before it runs share it.
    fn schedule(&self) {
        if !self.inbox.scheduled.swap(true, Ordering::AcqRel) {
            let _ = slint::invoke_from_event_loop(|| with_controller(Controller::drain));
        }
    }
}

impl Ui for SlintUi {
    fn log(&self, message: String) {
        self.file_log.write_line(&message);
        let line = format!("[{}] {message}", system::local_time_hms());
        self.inbox.logs.lock().unwrap().push(line);
        self.schedule();
    }

    fn state(&self, state: &StateDto) {
        *self.inbox.state.lock().unwrap() = Some(state.clone());
        self.schedule();
    }
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

fn with_controller(f: impl FnOnce(&mut Controller)) {
    CONTROLLER.with(|c| {
        if let Some(controller) = c.borrow_mut().as_mut() {
            f(controller);
        }
    });
}

/// Lives on the UI thread; owns the window models.
struct Controller {
    window: slint::Weak<AppWindow>,
    inbox: Arc<Inbox>,
    state: StateDto,
    log_model: Rc<VecModel<SharedString>>,
    worker_model: Rc<VecModel<WorkerRow>>,
    metric_model: Rc<VecModel<Metric>>,
    log_revision: i32,
    taskbar: Option<Taskbar>,
    taskbar_tried: bool,
}

impl Controller {
    fn drain(&mut self) {
        self.inbox.scheduled.store(false, Ordering::Release);
        let state = self.inbox.state.lock().unwrap().take();
        let logs = std::mem::take(&mut *self.inbox.logs.lock().unwrap());

        if !logs.is_empty() {
            for line in logs {
                self.log_model.push(line.into());
            }
            let excess = self.log_model.row_count().saturating_sub(MAX_LOG_LINES);
            for _ in 0..excess {
                self.log_model.remove(0);
            }
            self.log_revision += 1;
        }
        if let Some(state) = state {
            self.state = state;
        }
        self.render();
    }

    fn clear_log(&mut self) {
        self.log_model.set_vec(Vec::new());
        self.render();
    }

    fn render(&mut self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let s = &self.state;
        let busy = matches!(
            s.status,
            Status::Running | Status::Paused | Status::Stopping | Status::Scanning
        );
        let processing = matches!(s.status, Status::Running | Status::Paused | Status::Stopping);
        let paused = s.status == Status::Paused;
        let has_folder = s.dir.is_some();
        let has_run = s.run_total > 0 || s.completed_this_run > 0;
        let has_logs = self.log_model.row_count() > 0;

        w.set_status(status_key(s.status).into());
        w.set_status_label(status_label(s.status).into());
        w.set_has_folder(has_folder);
        w.set_folder_path(s.dir.as_deref().unwrap_or("No folder selected").into());
        w.set_folder_summary(if has_folder || s.status == Status::Scanning {
            file_count_text(s).into()
        } else {
            "Choose a folder to scan it for videos, including subfolders.".into()
        });
        w.set_busy(busy);
        w.set_processing(processing);
        w.set_can_start(has_folder && !busy && s.pending_files > 0);
        w.set_stop_enabled(matches!(s.status, Status::Running | Status::Paused));
        w.set_paused(paused);
        w.set_show_options(has_folder && !busy && s.pending_files > 0);
        w.set_show_stats(processing || has_run || s.status == Status::Error);
        w.set_show_log(processing || s.status == Status::Error || (has_run && has_logs));
        w.set_show_history(s.history_count > 0);
        w.set_history_text(history_text(s.history_count).into());
        w.set_log_revision(self.log_revision);

        // Metrics
        let t = &s.telemetry;
        let speed = if paused {
            "Paused".into()
        } else if processing && t.total_fps > 0 {
            format!("{} fps ({}x)", t.total_fps, t.total_speed)
        } else if processing {
            "Starting…".into()
        } else {
            "—".into()
        };
        let gpu = if processing {
            format!("{}% | {} GB", t.nvenc_util, t.vram_gb)
        } else {
            "0% | 0 GB".into()
        };
        let metrics = [
            ("Queued", s.pending_files.to_string(), "\u{E8FD}"),
            ("Completed", format!("{}/{}", s.completed_this_run, s.run_total), "\u{E73E}"),
            ("Space saved", format_saved(s.space_saved_mb), "\u{E74E}"),
            ("Speed", speed, "\u{EC4A}"),
            ("NVENC / VRAM", gpu, "\u{E950}"),
            ("Failed", s.failed_this_run.to_string(), "\u{E783}"),
        ];
        sync_model(
            &self.metric_model,
            metrics
                .into_iter()
                .map(|(label, value, glyph)| Metric {
                    label: label.into(),
                    value: value.into(),
                    glyph: glyph.into(),
                })
                .collect(),
        );

        // Progress
        let active = if processing { t.active_progress_sum } else { 0.0 };
        let fraction = if s.run_total > 0 {
            ((s.completed_this_run as f64 + active) / s.run_total as f64).min(1.0)
        } else {
            0.0
        };
        w.set_overall_progress(fraction as f32);
        w.set_overall_percent(format!("{}%", (fraction * 100.0).round()).into());
        w.set_current_file(if s.current_file.is_empty() {
            idle_file_text(s).as_str().into()
        } else {
            s.current_file.as_str().into()
        });
        let workers = if processing {
            t.workers
                .iter()
                .map(|wk| WorkerRow {
                    title: format!("Worker {} · {}", wk.worker_id, wk.file).into(),
                    meta: if wk.fps > 0 {
                        format!("{}% · {} fps ({}x)", wk.percent, wk.fps, wk.speed)
                    } else {
                        format!("{}%", wk.percent)
                    }
                    .into(),
                    progress: wk.percent as f32 / 100.0,
                })
                .collect()
        } else {
            Vec::new()
        };
        sync_model(&self.worker_model, workers);

        // Taskbar button progress
        let taskbar_pct = (fraction * 100.0).clamp(1.0, 99.0) as u64;
        let progress = if s.status == Status::Running && s.run_total > 0 {
            TaskbarProgress::Normal(taskbar_pct)
        } else if paused && s.run_total > 0 {
            TaskbarProgress::Paused(taskbar_pct)
        } else if s.status == Status::Error {
            TaskbarProgress::Error
        } else {
            TaskbarProgress::None
        };
        if !self.taskbar_tried {
            self.taskbar_tried = true;
            self.taskbar = hwnd(&w).and_then(Taskbar::new);
        }
        if let Some(taskbar) = &mut self.taskbar {
            taskbar.set(progress);
        }
    }
}

/// Updates rows in place when the length is unchanged (avoids rebuilding list items).
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    } else {
        model.set_vec(rows);
    }
}

fn hwnd(w: &AppWindow) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = w.window().window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

// ------------------------------------------------------------------- run

/// Command-line options: `batchviproc.exe [--start] [folder]`.
#[derive(Default)]
pub struct Launch {
    pub folder: Option<PathBuf>,
    pub start: bool,
}

impl Launch {
    pub fn from_args(args: impl Iterator<Item = String>) -> Self {
        let mut launch = Launch::default();
        for arg in args {
            if arg == "--start" {
                launch.start = true;
            } else if arg.starts_with("--") {
                // Unknown flag: ignore rather than treating it as a folder.
            } else if launch.folder.is_none() {
                launch.folder = Some(arg.into());
            }
        }
        launch
    }
}

pub fn run(data_dir: PathBuf, launch: Launch) -> Result<(), slint::PlatformError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("batchviproc-worker")
        .build()
        .expect("failed to start the async runtime");

    let window = AppWindow::new()?;
    window.global::<Theme>().set_icon_font(icon_font().into());

    let inbox = Arc::new(Inbox::default());
    let engine = Engine::new(
        Box::new(SlintUi {
            inbox: inbox.clone(),
            file_log: crate::logfile::FileLog::open(&data_dir.join("logs")),
        }),
        data_dir.clone(),
        crate::engine::locate_ffmpeg(),
        runtime.handle().clone(),
    );

    // Settings
    let prefs_path = data_dir.join("ui-settings.json");
    window.set_encoder_options(labels(ENCODERS));
    window.set_jobs_options(labels(JOBS));
    window.set_quality_options(labels(QUALITY));
    window.set_speed_options(labels(SPEEDS));
    let prefs = Prefs::load(&prefs_path);
    prefs.apply_to_window(&window);

    // Models
    let log_model = Rc::new(VecModel::<SharedString>::default());
    let worker_model = Rc::new(VecModel::<WorkerRow>::default());
    let metric_model = Rc::new(VecModel::<Metric>::default());
    window.set_log_lines(log_model.clone().into());
    window.set_workers(worker_model.clone().into());
    window.set_metrics(metric_model.clone().into());

    CONTROLLER.with(|c| {
        *c.borrow_mut() = Some(Controller {
            window: window.as_weak(),
            inbox,
            state: engine.snapshot(),
            log_model,
            worker_model,
            metric_model,
            log_revision: 0,
            taskbar: None,
            taskbar_tried: false,
        });
    });

    // Callbacks
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        window.on_choose_folder(move || {
            let Some(w) = weak.upgrade() else { return };
            let handle = w.window().window_handle();
            let picked = rfd::FileDialog::new()
                .set_title("Choose a folder with videos")
                .set_parent(&handle)
                .pick_folder();
            match picked {
                Some(dir) => engine.spawn_load_dir(dir),
                None => engine.log("No folder selected"),
            }
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_start(move || {
            let Some(w) = weak.upgrade() else { return };
            let prefs = Prefs::from_window(&w);
            prefs.save(&prefs_path);
            engine.start(prefs.to_raw());
        });
    }
    {
        let engine = engine.clone();
        window.on_stop(move || engine.stop());
    }
    {
        let engine = engine.clone();
        window.on_toggle_pause(move || engine.toggle_pause());
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        window.on_forget_history(move || {
            let Some(w) = weak.upgrade() else { return };
            let handle = w.window().window_handle();
            let confirmed = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("Forget processed history")
                .set_description(
                    "Forget the processed history for this app? Existing video files will not be changed.",
                )
                .set_buttons(rfd::MessageButtons::YesNo)
                .set_parent(&handle)
                .show()
                == rfd::MessageDialogResult::Yes;
            if confirmed {
                engine.clear_history();
            }
        });
    }
    window.on_clear_log(|| with_controller(Controller::clear_log));
    {
        // The full activity log and any crash reports live here.
        let logs_dir = data_dir.join("logs");
        window.on_open_logs(move || {
            let _ = std::fs::create_dir_all(&logs_dir);
            let _ = std::process::Command::new("explorer").arg(&logs_dir).spawn();
        });
    }
    {
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        let last_encoder = RefCell::new(prefs.encoder.clone());
        window.on_settings_changed(move || {
            let Some(w) = weak.upgrade() else { return };
            let mut prefs = Prefs::from_window(&w);
            // Switching to/from the CPU encoder adjusts parallel jobs.
            if *last_encoder.borrow() != prefs.encoder {
                if prefs.encoder == "libx264" {
                    prefs.concurrency = 1;
                } else if prefs.concurrency == 1 {
                    prefs.concurrency = 2;
                }
                prefs.apply_to_window(&w);
                *last_encoder.borrow_mut() = prefs.encoder.clone();
            }
            prefs.save(&prefs_path);
        });
    }

    // First paint, then honour command-line options.
    with_controller(Controller::render);
    if let Some(dir) = launch.folder {
        let engine = engine.clone();
        let raw = prefs.to_raw();
        runtime.spawn(async move {
            engine.load_dir(dir).await;
            if launch.start {
                engine.run_batch(raw).await;
            }
        });
    }

    let result = window.run();

    engine.shutdown();
    Prefs::from_window(&window).save(&prefs_path);
    CONTROLLER.with(|c| c.borrow_mut().take());
    drop(window);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// Windows 11 ships "Segoe Fluent Icons"; Windows 10 has the same glyphs in "Segoe MDL2 Assets".
fn icon_font() -> &'static str {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    if Path::new(&windir).join("Fonts").join("SegoeIcons.ttf").exists() {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    }
}

// ------------------------------------------------------------------- text helpers

fn status_key(status: Status) -> &'static str {
    match status {
        Status::Idle => "idle",
        Status::Scanning => "scanning",
        Status::Ready => "ready",
        Status::Running => "running",
        Status::Paused => "paused",
        Status::Stopping => "stopping",
        Status::Done => "done",
        Status::Error => "error",
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Idle => "Idle",
        Status::Scanning => "Scanning",
        Status::Ready => "Ready",
        Status::Running => "Running",
        Status::Paused => "Paused",
        Status::Stopping => "Stopping",
        Status::Done => "Done",
        Status::Error => "Needs attention",
    }
}

fn format_mb(value: f64) -> String {
    if value >= 10.0 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.1}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Space saved: MB below 1 GB, otherwise GB with one decimal.
fn format_saved(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{} MB", format_mb(mb))
    }
}

fn file_count_text(s: &StateDto) -> String {
    if s.status == Status::Scanning {
        return "Scanning folder…".into();
    }
    if s.total_files == 0 {
        return "No videos found".into();
    }
    let mut parts = vec![
        format!("{} videos found", s.total_files),
        format!("{} ready", s.pending_files),
    ];
    if s.skipped_known > 0 {
        parts.push(format!("{} already processed", s.skipped_known));
    }
    if s.skipped_generated > 0 {
        parts.push(format!("{} ignored", s.skipped_generated));
    }
    parts.join(" · ")
}

fn history_text(count: usize) -> String {
    if count == 1 {
        "1 processed file is remembered".into()
    } else {
        format!("{} processed files are remembered", group_thousands(count))
    }
}

/// 337426 -> "337,426"
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn idle_file_text(s: &StateDto) -> String {
    match s.status {
        Status::Done if s.run_total > 0 => format!(
            "Batch complete · {} compressed · {} skipped · {} failed",
            s.compressed_this_run, s.skipped_larger_this_run, s.failed_this_run
        ),
        Status::Done => "Batch complete".into(),
        Status::Ready if s.pending_files > 0 => "Ready to process".into(),
        Status::Error => "Check the activity log".into(),
        _ => "No active file".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_megabytes_like_the_old_ui() {
        assert_eq!(format_mb(0.0), "0");
        assert_eq!(format_mb(3.04), "3");
        assert_eq!(format_mb(3.46), "3.5");
        assert_eq!(format_mb(192.4), "192");
        assert_eq!(format_saved(192.4), "192 MB");
        assert_eq!(format_saved(1611.1), "1.6 GB");
    }

    #[test]
    fn groups_thousands_and_parses_launch_args() {
        assert_eq!(group_thousands(7), "7");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(337426), "337,426");
        let l = Launch::from_args(["--verbose", "--start", r"D:\Videos"].map(String::from).into_iter());
        assert!(l.start);
        assert_eq!(l.folder, Some(PathBuf::from(r"D:\Videos")));
    }

    #[test]
    fn unknown_saved_values_fall_back_to_defaults() {
        let d = Prefs::default();
        // e.g. the removed OpenCL encoder or an out-of-range job count
        assert_eq!(position(ENCODERS, &"libx264_opencl", &d.encoder.as_str()), 0);
        assert_eq!(position(JOBS, &9, &d.concurrency), 1);
        assert_eq!(position(SPEEDS, &"medium", &d.preset.as_str()), 1);
    }
}
