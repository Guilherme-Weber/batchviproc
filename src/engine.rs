//! Batch engine: folder scanning, the work queue, parallel ffmpeg workers with retries
//! and fallbacks, GPU telemetry and state broadcasting to the UI.

use crate::args::{self, Mode, Options, RawOptions};
use crate::history::History;
use crate::progress::{self, Diagnostics, WorkerProgress};
use crate::system::{self, OriginalAttributes, PowerGuard};

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio_util::sync::CancellationToken;

const STATE_THROTTLE: Duration = Duration::from_millis(250);
const HISTORY_SAVE_DELAY: Duration = Duration::from_secs(2);

/// Memory needed before starting another ffmpeg while others run. A single 4K AV1 encode
/// (multipass + 32-frame lookahead) was measured at up to ~5.3 GB of commit; running several
/// in parallel once exhausted the system commit limit and crashed every process.
const MIN_FREE_COMMIT: u64 = 6 * 1024 * 1024 * 1024;
const MIN_FREE_PHYSICAL: u64 = 2 * 1024 * 1024 * 1024;
/// How long a newly started ffmpeg is given to allocate its buffers before the next
/// worker re-checks free memory (only when memory is not plentiful).
const ADMISSION_WARMUP: Duration = Duration::from_secs(4);
const PLENTIFUL_COMMIT: u64 = 16 * 1024 * 1024 * 1024;

/// Whether another encoder may start now. The first encoder always may (otherwise a
/// low-memory machine would wait forever).
pub fn may_start_encoder(running: usize, memory: Option<system::MemoryStatus>) -> bool {
    match memory {
        _ if running == 0 => true,
        None => true,
        Some(m) => m.available_commit >= MIN_FREE_COMMIT && m.available_physical >= MIN_FREE_PHYSICAL,
    }
}

/// Where the engine sends logs and state (the window, or a recorder in tests).
pub trait Ui: Send + Sync + 'static {
    fn log(&self, message: String);
    fn state(&self, state: &StateDto);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Scanning,
    Ready,
    Running,
    Paused,
    Stopping,
    Done,
    Error,
}

#[derive(Default, Clone, Copy)]
struct RunStats {
    total: usize,
    completed: usize,
    compressed: usize,
    skipped_larger: usize,
    failed: usize,
}

#[derive(Clone, Debug)]
pub struct WorkerDto {
    pub worker_id: u32,
    pub file: String,
    pub fps: i64,
    pub speed: f64,
    pub percent: i64,
}

#[derive(Clone, Debug)]
pub struct TelemetryDto {
    pub total_fps: i64,
    pub total_speed: f64,
    pub active_progress_sum: f64,
    pub nvenc_util: f64,
    pub vram_gb: f64,
    pub workers: Vec<WorkerDto>,
}

#[derive(Clone, Debug)]
pub struct StateDto {
    pub status: Status,
    pub dir: Option<String>,
    pub total_files: usize,
    pub pending_files: usize,
    pub skipped_known: usize,
    pub skipped_generated: usize,
    pub history_count: usize,
    pub run_total: usize,
    pub completed_this_run: usize,
    pub compressed_this_run: usize,
    pub skipped_larger_this_run: usize,
    pub failed_this_run: usize,
    pub space_saved_mb: f64,
    pub current_file: String,
    pub telemetry: TelemetryDto,
}

struct Inner {
    dir: Option<PathBuf>,
    files: Vec<PathBuf>,
    pending_queue: Vec<PathBuf>,
    queue_cursor: usize,
    skipped_known: usize,
    skipped_generated: usize,
    history: History,
    history_save_pending: bool,
    failed: HashSet<PathBuf>,
    in_progress: HashSet<PathBuf>,
    /// Insertion-ordered list of files currently being encoded (for display).
    active: Vec<PathBuf>,
    workers: BTreeMap<u32, WorkerProgress>,
    running_processes: usize,
    gpu_enc: f64,
    gpu_vram_mb: f64,
    working: bool,
    stop_requested: bool,
    status: Status,
    space_saved_mb: f64,
    options: Options,
    run: RunStats,
    cancel: CancellationToken,
    power: Option<PowerGuard>,
}

struct EmitThrottle {
    last_sent: Option<Instant>,
    pending: bool,
}

pub struct Engine {
    ui: Box<dyn Ui>,
    /// Runtime that runs workers, ffmpeg I/O and timers (the UI thread stays free).
    rt: tokio::runtime::Handle,
    ffmpeg_path: PathBuf,
    inner: Mutex<Inner>,
    emit: Mutex<EmitThrottle>,
    /// Serializes encoder start-up so memory checks see the previous encoder's usage.
    admission: Arc<tokio::sync::Mutex<()>>,
    /// True while the batch is paused: running ffmpeg processes are suspended and no
    /// new file is started.
    paused: tokio::sync::watch::Sender<bool>,
}

enum Outcome {
    Processed,
    Skipped,
    Failed,
    Stopped,
    Empty,
    Fatal,
}

enum AttemptStatus {
    Exited(i32),
    Stopped,
    SpawnError(String),
}

struct Attempt {
    status: AttemptStatus,
    diag: Diagnostics,
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn is_generated_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.contains(".batchviproc.") || name.contains(".tmp.") || name.contains("-h264.")
}

fn is_orphan_temp(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase().contains(".batchviproc."))
        .unwrap_or(false)
}

/// Finds the ffmpeg binary: next to the executable (release zip), in the project's `bin/`
/// folder (development), otherwise on PATH.
pub fn locate_ffmpeg() -> PathBuf {
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe_name)));
    let dev_copy = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join(exe_name);
    beside_exe
        .into_iter()
        .chain(std::iter::once(dev_copy))
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(exe_name))
}

impl Inner {
    /// Rebuilds the work queue from the scanned files, history and this run's failures.
    fn rebuild_queue(&mut self) {
        let mut queue = Vec::new();
        let (mut known, mut generated) = (0, 0);
        for file in &self.files {
            if is_generated_name(file) {
                generated += 1;
            } else if self.history.contains(file) {
                known += 1;
            } else if !self.failed.contains(file) {
                queue.push(file.clone());
            }
        }
        self.pending_queue = queue;
        self.queue_cursor = 0;
        self.skipped_known = known;
        self.skipped_generated = generated;
    }

    fn remaining_pending(&self) -> usize {
        self.pending_queue.len().saturating_sub(self.queue_cursor) + self.in_progress.len()
    }

    fn display_name(&self, path: &Path) -> String {
        if let Some(dir) = &self.dir {
            if let Ok(rel) = path.strip_prefix(dir) {
                let rel = rel.to_string_lossy();
                if !rel.is_empty() {
                    return rel.into_owned();
                }
            }
        }
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn claim_next(&mut self) -> Option<PathBuf> {
        while self.queue_cursor < self.pending_queue.len() {
            let candidate = self.pending_queue[self.queue_cursor].clone();
            self.queue_cursor += 1;
            if !self.in_progress.contains(&candidate)
                && !self.failed.contains(&candidate)
                && !self.history.contains(&candidate)
            {
                self.in_progress.insert(candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    fn reset_run_tracking(&mut self) {
        self.failed.clear();
        self.in_progress.clear();
        self.active.clear();
        self.workers.clear();
        self.space_saved_mb = 0.0;
    }

    fn snapshot(&self) -> StateDto {
        let mut total_fps = 0.0;
        let mut total_speed = 0.0;
        let mut progress_sum = 0.0;
        // While paused, the last parsed fps/speed would be stale.
        let live = self.status != Status::Paused;
        let workers = self
            .workers
            .iter()
            .map(|(id, w)| {
                let (fps, speed) = if live { (w.fps, w.speed) } else { (0.0, 0.0) };
                total_fps += fps;
                total_speed += speed;
                let pct = w.percent.clamp(0.0, 100.0);
                progress_sum += pct / 100.0;
                WorkerDto {
                    worker_id: *id,
                    file: w.file.clone(),
                    fps: fps.round() as i64,
                    speed: round1(speed),
                    percent: pct.round() as i64,
                }
            })
            .collect();

        StateDto {
            status: self.status,
            dir: self.dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
            total_files: self.files.len(),
            pending_files: self.remaining_pending(),
            skipped_known: self.skipped_known,
            skipped_generated: self.skipped_generated,
            history_count: self.history.len(),
            run_total: self.run.total,
            completed_this_run: self.run.completed,
            compressed_this_run: self.run.compressed,
            skipped_larger_this_run: self.run.skipped_larger,
            failed_this_run: self.run.failed,
            space_saved_mb: round1(self.space_saved_mb),
            current_file: self
                .active
                .iter()
                .map(|p| self.display_name(p))
                .collect::<Vec<_>>()
                .join("  |  "),
            telemetry: TelemetryDto {
                total_fps: total_fps.round() as i64,
                total_speed: round1(total_speed),
                active_progress_sum: progress_sum,
                nvenc_util: self.gpu_enc,
                vram_gb: round1(self.gpu_vram_mb / 1024.0),
                workers,
            },
        }
    }
}

impl Engine {
    pub fn new(
        ui: Box<dyn Ui>,
        data_dir: PathBuf,
        ffmpeg_path: PathBuf,
        rt: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let history = History::load(data_dir.join("processedFiles.txt"));
        Arc::new(Self {
            ui,
            rt,
            ffmpeg_path,
            inner: Mutex::new(Inner {
                dir: None,
                files: Vec::new(),
                pending_queue: Vec::new(),
                queue_cursor: 0,
                skipped_known: 0,
                skipped_generated: 0,
                history,
                history_save_pending: false,
                failed: HashSet::new(),
                in_progress: HashSet::new(),
                active: Vec::new(),
                workers: BTreeMap::new(),
                running_processes: 0,
                gpu_enc: 0.0,
                gpu_vram_mb: 0.0,
                working: false,
                stop_requested: false,
                status: Status::Idle,
                space_saved_mb: 0.0,
                options: Options::default(),
                run: RunStats::default(),
                cancel: CancellationToken::new(),
                power: None,
            }),
            emit: Mutex::new(EmitThrottle {
                last_sent: None,
                pending: false,
            }),
            admission: Arc::new(tokio::sync::Mutex::new(())),
            paused: tokio::sync::watch::Sender::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs blocking disk work off the async worker threads.
    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> T {
        self.rt
            .spawn_blocking(f)
            .await
            .expect("blocking file task panicked")
    }

    pub fn log(&self, message: impl Into<String>) {
        self.ui.log(message.into());
    }

    pub fn snapshot(&self) -> StateDto {
        self.lock().snapshot()
    }

    /// Broadcasts state to the UI; non-forced updates are throttled to one per 250 ms.
    pub fn send_state(self: &Arc<Self>, force: bool) {
        {
            let mut emit = self.emit.lock().unwrap_or_else(|e| e.into_inner());
            let recent = emit
                .last_sent
                .is_some_and(|t| t.elapsed() < STATE_THROTTLE);
            if !force && recent {
                if !emit.pending {
                    emit.pending = true;
                    let engine = Arc::clone(self);
                    self.rt.spawn(async move {
                        tokio::time::sleep(STATE_THROTTLE).await;
                        engine.send_state(true);
                    });
                }
                return;
            }
            emit.pending = false;
            emit.last_sent = Some(Instant::now());
        }

        let state = self.snapshot();
        self.ui.state(&state);
    }

    /// Saves history after a short delay so bursts of completions cause one write.
    fn schedule_history_save(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.history_save_pending {
                return;
            }
            inner.history_save_pending = true;
        }
        let engine = Arc::clone(self);
        self.rt.spawn(async move {
            tokio::time::sleep(HISTORY_SAVE_DELAY).await;
            engine.flush_history();
        });
    }

    pub fn flush_history(&self) {
        let mut inner = self.lock();
        inner.history_save_pending = false;
        if let Err(err) = inner.history.save() {
            eprintln!("Error saving processed files: {err}");
        }
    }

    fn add_processed(self: &Arc<Self>, file: &Path) {
        let added = {
            let mut inner = self.lock();
            let added = inner.history.add(file);
            if added {
                inner.skipped_known += 1;
            }
            added
        };
        if added {
            self.schedule_history_save();
        }
    }

    // ---------------------------------------------------------------- folder selection

    pub fn is_working(&self) -> bool {
        self.lock().working
    }

    /// Selects a folder and scans it for videos.
    pub async fn load_dir(self: &Arc<Self>, dir: PathBuf) {
        if self.is_working() {
            self.log("Stop processing before choosing another folder");
            return;
        }
        if !dir.is_dir() {
            self.log(format!("Folder not found: {}", dir.display()));
            return;
        }

        {
            let mut inner = self.lock();
            inner.dir = Some(dir.clone());
            inner.files.clear();
            inner.pending_queue.clear();
            inner.queue_cursor = 0;
            inner.reset_run_tracking();
            inner.run = RunStats::default();
            inner.status = Status::Scanning;
        }
        self.log(format!("Scanning {}", dir.display()));
        self.send_state(true);

        let scan_dir = dir.clone();
        let scan = self.rt.spawn_blocking(move || scan_folder(&scan_dir)).await;

        match scan {
            Ok(result) => {
                for msg in &result.messages {
                    self.log(msg.clone());
                }
                if result.unreadable > 0 {
                    self.log(format!(
                        "{} files or folders could not be read (permissions or broken links) and were skipped",
                        result.unreadable
                    ));
                }
                let (known, generated) = {
                    let mut inner = self.lock();
                    inner.files = result.files;
                    inner.reset_run_tracking();
                    inner.rebuild_queue();
                    inner.run = RunStats::default();
                    inner.status = Status::Ready;
                    (inner.skipped_known, inner.skipped_generated)
                };
                self.log(format!(
                    "Found {} videos/GIFs in {}",
                    self.lock().files.len(),
                    dir.display()
                ));
                if result.cleaned_orphans > 0 {
                    self.log(format!(
                        "Removed {} leftover temporary (.batchviproc) file(s)",
                        result.cleaned_orphans
                    ));
                }
                if known > 0 {
                    self.log(format!("{known} videos are already in the processed history"));
                }
                if generated > 0 {
                    self.log(format!(
                        "{generated} generated or temporary files will be ignored"
                    ));
                }
            }
            Err(err) => {
                self.lock().status = Status::Error;
                self.log(format!("Could not scan folder: {err}"));
            }
        }
        self.send_state(true);
    }

    // ---------------------------------------------------------------- batch control

    /// Scans a folder in the background.
    pub fn spawn_load_dir(self: &Arc<Self>, dir: PathBuf) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.load_dir(dir).await });
    }

    pub fn start(self: &Arc<Self>, raw: RawOptions) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.run_batch(raw).await });
    }

    pub async fn run_batch(self: Arc<Self>, raw: RawOptions) {
        let (total, options) = {
            let mut inner = self.lock();
            if inner.dir.is_none() {
                drop(inner);
                self.log("Select a folder before starting");
                self.send_state(true);
                return;
            }
            if inner.working {
                drop(inner);
                self.log("Processing is already running");
                self.send_state(true);
                return;
            }
            inner.options = Options::normalize(raw);
            inner.reset_run_tracking();
            inner.rebuild_queue();
            inner.stop_requested = false;
            self.paused.send_replace(false);
            inner.run = RunStats {
                total: inner.pending_queue.len(),
                ..RunStats::default()
            };
            if inner.run.total > 0 {
                inner.working = true;
                inner.status = Status::Running;
                inner.cancel = CancellationToken::new();
                inner.power = Some(PowerGuard::acquire());
            } else {
                inner.status = Status::Done;
            }
            (inner.run.total, inner.options.clone())
        };

        if total == 0 {
            self.log("No new videos to process");
            self.send_state(true);
            return;
        }

        self.start_gpu_polling();
        self.log(format!(
            "Starting {total} videos | Encoder: {} | Workers: {} | Base CRF: {} | Preset: {}",
            options.encoder, options.concurrency, options.crf, options.preset
        ));
        self.send_state(true);

        let fatal = Arc::new(AtomicBool::new(false));
        let worker_count = (options.concurrency as usize).min(total);
        let handles: Vec<_> = (1..=worker_count as u32)
            .map(|id| {
                let engine = Arc::clone(&self);
                let fatal = Arc::clone(&fatal);
                self.rt.spawn(async move { engine.run_worker(id, fatal).await })
            })
            .collect();
        for handle in handles {
            let _ = handle.await;
        }

        let (was_stopped, status, run, saved) = {
            let mut inner = self.lock();
            let was_stopped = inner.stop_requested;
            inner.working = false;
            inner.stop_requested = false;
            self.paused.send_replace(false);
            inner.power = None;
            inner.gpu_enc = 0.0;
            inner.gpu_vram_mb = 0.0;
            inner.active.clear();
            inner.in_progress.clear();
            inner.workers.clear();
            inner.rebuild_queue();
            if was_stopped {
                inner.status = Status::Ready;
            } else if inner.status != Status::Error {
                inner.status = Status::Done;
            }
            (was_stopped, inner.status, inner.run, round1(inner.space_saved_mb))
        };
        self.flush_history();

        if was_stopped {
            self.log(format!(
                "Stopped. Completed {}/{}; saved {saved} MB",
                run.completed, run.total
            ));
        } else if status == Status::Error {
            self.log(format!("Stopped after an ffmpeg startup error; saved {saved} MB"));
        } else {
            self.log(format!(
                "Done. Compressed {}, skipped {}, failed {}. Saved {saved} MB total",
                run.compressed, run.skipped_larger, run.failed
            ));
        }
        self.send_state(true);
    }

    async fn run_worker(self: Arc<Self>, worker_id: u32, fatal: Arc<AtomicBool>) {
        loop {
            {
                let inner = self.lock();
                if !inner.working || inner.stop_requested || fatal.load(Ordering::SeqCst) {
                    break;
                }
            }
            match self.process_one(worker_id).await {
                Outcome::Empty | Outcome::Stopped => break,
                Outcome::Fatal => {
                    fatal.store(true, Ordering::SeqCst);
                    self.lock().status = Status::Error;
                    break;
                }
                Outcome::Processed | Outcome::Skipped | Outcome::Failed => {}
            }
        }
    }

    pub fn stop(self: &Arc<Self>) {
        let running = {
            let mut inner = self.lock();
            if !inner.working {
                None
            } else {
                inner.stop_requested = true;
                inner.status = Status::Stopping;
                inner.cancel.cancel();
                Some(inner.running_processes)
            }
        };
        if let Some(n) = running {
            self.log(format!("Stopping {n} active ffmpeg process(es)..."));
        }
        self.send_state(true);
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// Pauses or resumes the running batch. Pausing suspends the active ffmpeg processes
    /// (they keep their progress), starts no new files and lets the PC sleep again.
    pub fn set_paused(self: &Arc<Self>, pause: bool) {
        let active = {
            let mut inner = self.lock();
            let allowed = if pause {
                inner.status == Status::Running
            } else {
                inner.status == Status::Paused
            };
            if !inner.working || inner.stop_requested || !allowed {
                return;
            }
            if pause {
                inner.status = Status::Paused;
                inner.power = None;
            } else {
                inner.status = Status::Running;
                inner.power = Some(PowerGuard::acquire());
            }
            self.paused.send_replace(pause);
            inner.running_processes
        };
        self.log(if pause {
            format!("Paused ({active} ffmpeg process(es) suspended). Press Resume to continue.")
        } else {
            "Resumed".to_string()
        });
        self.send_state(true);
    }

    pub fn toggle_pause(self: &Arc<Self>) {
        self.set_paused(!self.is_paused());
    }

    /// Waits while the batch is paused. Returns false if Stop was pressed.
    async fn wait_while_paused(&self, cancel: &CancellationToken) -> bool {
        let mut rx = self.paused.subscribe();
        tokio::select! {
            r = rx.wait_for(|paused| !*paused) => r.is_ok(),
            _ = cancel.cancelled() => false,
        }
    }

    pub fn clear_history(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.working {
                drop(inner);
                self.log("Stop processing before clearing processed history");
                return;
            }
            inner.history.clear();
            inner.rebuild_queue();
            inner.status = if inner.dir.is_some() {
                Status::Ready
            } else {
                Status::Idle
            };
        }
        self.flush_history();
        self.log("Processed history cleared");
        self.send_state(true);
    }

    /// Called when the app exits: stops ffmpeg and saves history.
    pub fn shutdown(&self) {
        {
            let mut inner = self.lock();
            inner.stop_requested = true;
            inner.cancel.cancel();
            inner.power = None;
        }
        self.flush_history();
    }

    /// Reads NVENC load and VRAM from one long-running `nvidia-smi -lms` process
    /// (spawning nvidia-smi for every sample cost ~50 ms of CPU each time).
    fn start_gpu_polling(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        let cancel = self.lock().cancel.clone();
        self.rt.spawn(async move {
            let mut cmd = tokio::process::Command::new("nvidia-smi");
            cmd.args([
                "--query-gpu=utilization.encoder,memory.used",
                "--format=csv,noheader,nounits",
                "-lms",
                "1500",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
            system::configure_background_command(&mut cmd, false);
            let Ok(mut child) = cmd.spawn() else {
                return; // no NVIDIA driver: telemetry stays at zero
            };
            system::assign_to_kill_on_exit_job(&child);
            let Some(stdout) = child.stdout.take() else {
                return;
            };
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let mut parts = line.split(',');
                        let enc = parts.next().and_then(|v| v.trim().parse::<f64>().ok());
                        let vram = parts.next().and_then(|v| v.trim().parse::<f64>().ok());
                        if let Some(enc) = enc {
                            {
                                let mut inner = engine.lock();
                                if !inner.working {
                                    break;
                                }
                                inner.gpu_enc = enc;
                                inner.gpu_vram_mb = vram.unwrap_or(0.0);
                            }
                            engine.send_state(false);
                        }
                    }
                    _ = cancel.cancelled() => break,
                }
            }
            let _ = child.start_kill();
        });
    }

    // ---------------------------------------------------------------- per-file pipeline

    fn reset_worker(&self, worker_id: u32, label: String) {
        self.lock()
            .workers
            .insert(worker_id, WorkerProgress::new(label));
    }

    fn release_tracking(&self, worker_id: u32, input: &Path) {
        let mut inner = self.lock();
        inner.active.retain(|p| p != input);
        inner.in_progress.remove(input);
        inner.workers.remove(&worker_id);
    }

    fn log_remove(&self, path: &Path) {
        if let Err(err) = system::remove_file_if_exists(path) {
            self.log(format!(
                "Could not remove temporary file {}: {err}",
                path.display()
            ));
        }
    }

    async fn process_one(self: &Arc<Self>, worker_id: u32) -> Outcome {
        let cancel = self.lock().cancel.clone();
        if !self.wait_while_paused(&cancel).await {
            return Outcome::Stopped;
        }
        let claimed = {
            let mut inner = self.lock();
            if !inner.working || inner.stop_requested {
                return Outcome::Stopped;
            }
            inner
                .claim_next()
                .map(|input| (inner.display_name(&input), input))
        };
        let Some((display, input)) = claimed else {
            return Outcome::Empty;
        };

        // Skip static 1-frame GIFs and tiny spacer GIFs immediately.
        if args::is_gif(&input) {
            let path = input.clone();
            let valid = self
                .blocking(move || {
                    std::fs::File::open(&path)
                        .map(|f| system::is_animated_and_valid_gif(std::io::BufReader::new(f)))
                        .unwrap_or(false)
                })
                .await;
            if !valid {
                self.lock().in_progress.remove(&input);
                self.add_processed(&input);
                {
                    let mut inner = self.lock();
                    inner.run.completed += 1;
                    inner.run.skipped_larger += 1;
                }
                self.log(format!(
                    "[Worker {worker_id}] Skipped static or tiny spacer GIF: {display}"
                ));
                self.send_state(false);
                return Outcome::Skipped;
            }
        }

        let tmp = args::temp_name(&input, worker_id);
        self.lock().active.push(input.clone());
        self.reset_worker(worker_id, display.clone());
        self.log_remove(&tmp);
        self.send_state(true);
        self.log(format!("[Worker {worker_id}] Processing: {display}"));

        let initial_mode = if args::needs_immediate_audio_transcode(&input) {
            Mode::TranscodeAudio
        } else {
            Mode::Primary
        };
        let mut attempt = self
            .run_attempt(worker_id, &input, &tmp, &display, initial_mode)
            .await;

        // Audio-only file (e.g. a voice note in .mp4): skip and remember.
        if attempt.diag.no_video_stream {
            self.release_tracking(worker_id, &input);
            self.log_remove(&tmp);
            self.add_processed(&input);
            {
                let mut inner = self.lock();
                inner.run.completed += 1;
                inner.run.skipped_larger += 1;
            }
            self.log(format!(
                "[Worker {worker_id}] Skipped audio-only file (no video stream): {display}"
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }

        // Corrupt or incomplete file: remember it so it is never retried.
        if attempt.diag.corrupt_input {
            self.release_tracking(worker_id, &input);
            self.log_remove(&tmp);
            self.add_processed(&input);
            {
                let mut inner = self.lock();
                inner.run.completed += 1;
                inner.run.failed += 1;
            }
            self.log(format!(
                "[Worker {worker_id}] Corrupt/unreadable file remembered in history (will not retry): {display}"
            ));
            self.send_state(true);
            return Outcome::Failed;
        }

        let can_retry = |engine: &Engine, a: &Attempt| {
            let inner = engine.lock();
            matches!(a.status, AttemptStatus::Exited(code) if code != 0)
                && !a.diag.corrupt_input
                && !a.diag.no_video_stream
                && !inner.stop_requested
                && inner.working
        };
        let encoder_is_nvenc = self.lock().options.encoder.ends_with("_nvenc");
        let mut tried_cpu = false;

        // Tiny GIF/sticker or stream rejected by the NVENC hardware -> CPU x264.
        if can_retry(self, &attempt) && attempt.diag.nvenc_unsupported {
            tried_cpu = true;
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] NVENC hardware limit on {display}; falling back to CPU software encoder..."
            ));
            self.reset_worker(worker_id, format!("{display} (CPU fallback)"));
            attempt = self
                .run_attempt(worker_id, &input, &tmp, &display, Mode::CpuFallback)
                .await;
        }

        // Audio stream copy rejected by the muxer -> transcode audio to AAC.
        if can_retry(self, &attempt)
            && !tried_cpu
            && initial_mode == Mode::Primary
            && !args::is_gif(&input)
        {
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Stream copy failed on {display}; retrying with AAC audio transcode..."
            ));
            self.reset_worker(worker_id, format!("{display} (AAC fallback)"));
            attempt = self
                .run_attempt(worker_id, &input, &tmp, &display, Mode::TranscodeAudio)
                .await;
        }

        // Final safety net: CPU fallback for NVENC encoders.
        if can_retry(self, &attempt) && !tried_cpu && encoder_is_nvenc {
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Hardware pipeline failed on {display}; attempting final CPU software fallback..."
            ));
            self.reset_worker(worker_id, format!("{display} (CPU fallback)"));
            attempt = self
                .run_attempt(worker_id, &input, &tmp, &display, Mode::CpuFallback)
                .await;
        }

        self.release_tracking(worker_id, &input);

        let stop_requested = self.lock().stop_requested;
        if matches!(attempt.status, AttemptStatus::Stopped) || stop_requested {
            self.log_remove(&tmp);
            self.log(format!("[Worker {worker_id}] Stopped while processing {display}"));
            self.send_state(true);
            return Outcome::Stopped;
        }

        let code = match attempt.status {
            AttemptStatus::SpawnError(err) => {
                {
                    let mut inner = self.lock();
                    inner.failed.insert(input.clone());
                    inner.run.completed += 1;
                    inner.run.failed += 1;
                }
                self.log_remove(&tmp);
                self.log(format!(
                    "[Worker {worker_id}] Could not start ffmpeg ({}): {err}",
                    self.ffmpeg_path.display()
                ));
                self.send_state(true);
                return Outcome::Fatal;
            }
            AttemptStatus::Exited(code) => code,
            AttemptStatus::Stopped => unreachable!(),
        };

        self.lock().run.completed += 1;

        if code != 0 {
            self.lock().failed.insert(input.clone());
            self.add_processed(&input); // Remember unrecoverable files so future runs skip them
            self.lock().run.failed += 1;
            self.log_remove(&tmp);
            for line in &attempt.diag.error_lines {
                self.log(line.clone());
            }
            self.log(format!(
                "[Worker {worker_id}] ffmpeg exited with code {code} for {display} (remembered in history)"
            ));
            self.send_state(true);
            return Outcome::Failed;
        }

        let (in_path, tmp_path) = (input.clone(), tmp.clone());
        let metas = self
            .blocking(move || (std::fs::metadata(&in_path), std::fs::metadata(&tmp_path)))
            .await;
        let (in_meta, out_meta) = match metas {
            (Ok(i), Ok(o)) => (i, o),
            (Err(err), _) | (_, Err(err)) => {
                {
                    let mut inner = self.lock();
                    inner.failed.insert(input.clone());
                    inner.run.failed += 1;
                }
                self.log_remove(&tmp);
                self.log(format!(
                    "Could not inspect processed output for {display}: {err}"
                ));
                self.send_state(true);
                return Outcome::Failed;
            }
        };

        let in_size = in_meta.len() as f64;
        let out_size = out_meta.len() as f64;
        let saved_mb = (in_size - out_size) / (1024.0 * 1024.0);
        let saved_pct = if in_size > 0.0 {
            ((in_size - out_size) / in_size * 100.0).round() as i64
        } else {
            0
        };

        if saved_mb <= 0.0 {
            self.add_processed(&input);
            self.lock().run.skipped_larger += 1;
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Output was not smaller for {display}; kept the original file"
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }

        let in_ext = args::ext_lower(&input);
        let tmp_ext = args::ext_lower(&tmp);
        let final_target = if in_ext != tmp_ext {
            let stem = input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = tmp
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            input.with_file_name(format!("{stem}{ext}"))
        } else {
            input.clone()
        };

        if final_target != input && final_target.exists() {
            self.add_processed(&input);
            self.lock().run.skipped_larger += 1;
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Target {} already exists; kept original {display}",
                final_target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }

        let attrs = OriginalAttributes::from_metadata(&in_meta);
        let (tmp_c, target_c, input_c) = (tmp.clone(), final_target.clone(), input.clone());
        let replaced = self
            .blocking(move || system::replace_file_with_retry(&tmp_c, &target_c, &input_c))
            .await;
        if let Err(err) = replaced {
            {
                let mut inner = self.lock();
                inner.failed.insert(input.clone());
                inner.run.failed += 1;
            }
            self.log_remove(&tmp);
            self.log(format!("Could not replace {display}: {err}"));
            self.send_state(true);
            return Outcome::Failed;
        }

        self.add_processed(&input);
        {
            let mut inner = self.lock();
            if final_target != input {
                inner.history.add(&final_target);
            }
            inner.run.compressed += 1;
            inner.space_saved_mb += saved_mb;
        }

        let target_c = final_target.clone();
        let restored = self
            .blocking(move || system::restore_attributes(&target_c, &attrs))
            .await;
        if let Err(err) = restored {
            self.log(format!("Could not restore dates/attributes for {display}: {err}"));
        }

        self.log(format!(
            "[Worker {worker_id}] Saved {} MB ({saved_pct}%) from {display}",
            round1(saved_mb)
        ));
        self.send_state(false);
        Outcome::Processed
    }

    /// Waits until another encoder fits in memory. Returns false if Stop was pressed.
    async fn wait_for_memory(&self, worker_id: u32, display: &str, cancel: &CancellationToken) -> bool {
        let mut announced = false;
        loop {
            let running = self.lock().running_processes;
            let memory = system::memory_status();
            if may_start_encoder(running, memory) {
                if announced {
                    self.reset_worker(worker_id, display.to_string());
                    self.log(format!("[Worker {worker_id}] Memory available again; starting {display}"));
                }
                return true;
            }
            if !announced {
                announced = true;
                let free_gb = memory.map_or(0.0, |m| m.available_commit as f64 / 1_073_741_824.0);
                self.log(format!(
                    "[Worker {worker_id}] Waiting for free memory before starting {display} ({free_gb:.1} GB available)"
                ));
                self.reset_worker(worker_id, format!("{display} (waiting for memory)"));
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = cancel.cancelled() => return false,
            }
        }
    }

    /// Runs ffmpeg once, streaming its stderr into the worker's progress, with a stall
    /// watchdog and cancellation on Stop.
    async fn run_attempt(
        self: &Arc<Self>,
        worker_id: u32,
        input: &Path,
        tmp: &Path,
        display: &str,
        mode: Mode,
    ) -> Attempt {
        let mut diag = Diagnostics::default();
        let (options, cancel) = {
            let inner = self.lock();
            (inner.options.clone(), inner.cancel.clone())
        };
        let ffmpeg_args = args::build_ffmpeg_args(input, tmp, &options, mode);

        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(&ffmpeg_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        system::configure_background_command(&mut cmd, true);

        // Wait for enough free memory before adding another encoder.
        // Encoders start one at a time; show why a worker sits at 0% meanwhile.
        let permit = match self.admission.clone().try_lock_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.reset_worker(worker_id, format!("{display} (waiting to start)"));
                self.send_state(false);
                let permit = tokio::select! {
                    permit = self.admission.clone().lock_owned() => permit,
                    _ = cancel.cancelled() => {
                        return Attempt { status: AttemptStatus::Stopped, diag };
                    }
                };
                self.reset_worker(worker_id, display.to_string());
                permit
            }
        };
        // A retry (AAC / CPU fallback) may come up while the batch is paused.
        if self.is_paused() {
            self.reset_worker(worker_id, format!("{display} (paused)"));
            self.send_state(false);
            if !self.wait_while_paused(&cancel).await {
                return Attempt { status: AttemptStatus::Stopped, diag };
            }
            self.reset_worker(worker_id, display.to_string());
        }
        if !self.wait_for_memory(worker_id, display, &cancel).await {
            return Attempt {
                status: AttemptStatus::Stopped,
                diag,
            };
        }
        let mut pause_rx = self.paused.subscribe();

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                return Attempt {
                    status: AttemptStatus::SpawnError(err.to_string()),
                    diag,
                }
            }
        };
        system::assign_to_kill_on_exit_job(&child);
        self.lock().running_processes += 1;

        // Give the new encoder time to allocate before the next one checks memory, unless
        // memory is plentiful. The hold ends early at the first progress line (buffers are
        // allocated by then) or when ffmpeg exits, so short files keep running in parallel.
        let warmup_done = CancellationToken::new();
        let plentiful = system::memory_status().is_some_and(|m| m.available_commit >= PLENTIFUL_COMMIT);
        if plentiful {
            drop(permit);
        } else {
            let done = warmup_done.clone();
            self.rt.spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(ADMISSION_WARMUP) => {}
                    _ = done.cancelled() => {}
                }
                drop(permit);
            });
        }

        let mut started = Instant::now();
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let mut buf = vec![0u8; 16 * 1024];
        let mut watchdog = tokio::time::interval(Duration::from_secs(5));
        watchdog.tick().await;
        let mut killed_for_stop = false;
        let mut killed_for_stall = false;
        // Pause pressed between the check above and the spawn.
        let mut suspended =
            *pause_rx.borrow_and_update() && system::set_process_suspended(&child, true);

        loop {
            tokio::select! {
                changed = pause_rx.changed() => {
                    if changed.is_err() {
                        continue; // the engine owns the sender; cannot happen
                    }
                    let pause = *pause_rx.borrow_and_update();
                    if pause && !suspended {
                        suspended = system::set_process_suspended(&child, true);
                    } else if !pause && suspended {
                        system::set_process_suspended(&child, false);
                        suspended = false;
                        // Time spent paused must not count as a stall.
                        started = Instant::now();
                        if let Some(w) = self.lock().workers.get_mut(&worker_id) {
                            w.last_progress_at = Instant::now();
                        }
                        watchdog.reset();
                    }
                }
                read = stderr.read(&mut buf) => {
                    let n = match read {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    let (saw_progress, encoding) = {
                        let mut inner = self.lock();
                        let progress = inner
                            .workers
                            .entry(worker_id)
                            .or_insert_with(|| WorkerProgress::new(display));
                        let saw = progress::parse_chunk(progress, &mut diag, worker_id, display, &chunk);
                        (saw, progress.last_encoded_sec > 0.0)
                    };
                    if saw_progress {
                        // Only once frames are actually coming out (time= advancing) are the
                        // encoder's buffers and lookahead fully allocated.
                        if encoding {
                            warmup_done.cancel();
                        }
                        self.send_state(false);
                    }
                }
                _ = watchdog.tick(), if !killed_for_stall && !suspended => {
                    let (idle, percent) = {
                        let inner = self.lock();
                        inner.workers.get(&worker_id).map_or(
                            (started.elapsed(), 0.0),
                            |w| (w.last_progress_at.elapsed(), w.percent),
                        )
                    };
                    let limit = if percent == 0.0 { 30 } else { 60 };
                    if idle > Duration::from_secs(limit) {
                        self.log(format!(
                            "[Worker {worker_id}] Stall watchdog triggered on {display} ({}s without frame progress). Terminating process.",
                            idle.as_secs()
                        ));
                        killed_for_stall = true;
                        let _ = child.start_kill();
                    }
                }
                _ = cancel.cancelled(), if !killed_for_stop => {
                    killed_for_stop = true;
                    let _ = child.start_kill();
                }
            }
        }

        warmup_done.cancel();
        let exit = child.wait().await;
        self.lock().running_processes -= 1;

        let status = if killed_for_stop || self.lock().stop_requested {
            AttemptStatus::Stopped
        } else {
            match exit {
                Ok(s) => AttemptStatus::Exited(s.code().unwrap_or(1)),
                Err(err) => AttemptStatus::SpawnError(err.to_string()),
            }
        };
        Attempt { status, diag }
    }
}

struct ScanResult {
    files: Vec<PathBuf>,
    cleaned_orphans: usize,
    unreadable: usize,
    messages: Vec<String>,
}

/// Recursively finds videos (skipping hidden folders like the old glob did), deletes leftover
/// temp files and sorts the queue by size, largest first. File sizes come from the directory
/// listing itself (on Windows `DirEntry::metadata` needs no extra system call).
fn scan_folder(dir: &Path) -> ScanResult {
    let mut messages = Vec::new();
    let mut cleaned_orphans = 0;
    let mut unreadable = 0;
    let mut with_sizes = Vec::new();

    let walker = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() || !args::is_video_file(entry.path()) {
            continue;
        }
        if is_orphan_temp(entry.path()) {
            match system::remove_file_if_exists(entry.path()) {
                Ok(()) => cleaned_orphans += 1,
                Err(err) => messages.push(format!(
                    "Could not remove temporary file {}: {err}",
                    entry.path().display()
                )),
            }
            continue;
        }
        match entry.metadata() {
            Ok(meta) if meta.len() > 0 => with_sizes.push((entry.into_path(), meta.len())),
            Ok(_) => {}
            Err(err) => messages.push(format!("Could not read {}: {err}", entry.path().display())),
        }
    }

    with_sizes.sort_by_key(|&(_, size)| std::cmp::Reverse(size));
    ScanResult {
        files: with_sizes.into_iter().map(|(p, _)| p).collect(),
        cleaned_orphans,
        unreadable,
        messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn mem(commit_gb: u64, phys_gb: u64) -> Option<system::MemoryStatus> {
        Some(system::MemoryStatus {
            available_commit: commit_gb * GB,
            available_physical: phys_gb * GB,
        })
    }

    #[test]
    fn memory_admission() {
        // The first encoder always starts, even when memory is tight.
        assert!(may_start_encoder(0, mem(1, 1)));
        // Further encoders need headroom for a 4K encode.
        assert!(!may_start_encoder(1, mem(4, 8)));
        assert!(!may_start_encoder(1, mem(10, 1)));
        assert!(may_start_encoder(1, mem(10, 4)));
        assert!(may_start_encoder(3, None));
    }

    #[test]
    fn generated_and_orphan_names() {
        assert!(is_generated_name(Path::new(r"C:\v\a.batchviproc.w1.mp4")));
        assert!(is_generated_name(Path::new(r"C:\v\clip.TMP.mp4")));
        assert!(is_generated_name(Path::new(r"C:\v\clip-H264.mkv")));
        assert!(!is_generated_name(Path::new(r"C:\v\clip.mp4")));
        assert!(is_orphan_temp(Path::new(r"C:\v\x.BatchViProc.w2.mp4")));
    }
}
