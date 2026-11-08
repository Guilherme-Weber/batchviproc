//! End-to-end tests of the engine with the bundled ffmpeg and a real GPU.
//! They are skipped when bin/ffmpeg.exe is missing (run scripts/fetch-ffmpeg.ps1).

use crate::args::RawOptions;
use crate::engine::{Engine, StateDto, Status, Ui};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Default)]
struct Recorder {
    logs: Mutex<Vec<String>>,
}

struct RecorderUi(Arc<Recorder>);

impl Ui for RecorderUi {
    fn log(&self, message: String) {
        self.0.logs.lock().unwrap().push(message);
    }
    fn state(&self, _state: &StateDto) {}
}

fn ffmpeg() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
    p.exists().then_some(p)
}

fn make(ffmpeg: &Path, args: &[&str], out: &Path) {
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(out)
        .status()
        .unwrap();
    assert!(status.success(), "could not create {}", out.display());
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("batchviproc-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn lossless_video(ffmpeg: &Path, seconds: u32, size: &str, out: &Path) {
    let src = format!("testsrc2=size={size}:rate=30:duration={seconds}");
    let sine = format!("sine=duration={seconds}");
    make(
        ffmpeg,
        &["-f", "lavfi", "-i", &src, "-f", "lavfi", "-i", &sine, "-c:v", "libx264", "-preset",
          "ultrafast", "-qp", "0", "-c:a", "aac", "-shortest"],
        out,
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn new_engine(
    rt: &tokio::runtime::Runtime,
    root: &Path,
    ffmpeg: PathBuf,
) -> (Arc<Engine>, Arc<Recorder>) {
    let rec = Arc::new(Recorder::default());
    let ui = Box::new(RecorderUi(rec.clone()));
    let engine = Engine::new(ui, root.join("appdata"), ffmpeg, rt.handle().clone());
    (engine, rec)
}

fn opts(encoder: &str, concurrency: f64) -> RawOptions {
    RawOptions {
        crf: Some(23.0),
        preset: Some("slow".into()),
        encoder: Some(encoder.into()),
        concurrency: Some(concurrency),
    }
}

#[test]
fn full_batch_handles_every_file_kind() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("full");
    let videos = root.join("videos");
    std::fs::create_dir_all(videos.join("sub")).unwrap();
    std::fs::create_dir_all(videos.join(".hidden")).unwrap();

    let big = videos.join("big.mp4");
    lossless_video(&ff, 4, "1280x720", &big);
    let mov = videos.join("sub").join("clip.mov");
    lossless_video(&ff, 2, "640x360", &mov);
    make(&ff, &["-f", "lavfi", "-i", "sine=duration=2", "-c:a", "aac"], &videos.join("voice.mp4"));
    std::fs::write(videos.join("broken.mp4"), vec![7u8; 50_000]).unwrap();
    make(&ff, &["-f", "lavfi", "-i", "color=red:size=64x64:duration=0.04", "-frames:v", "1"], &videos.join("static.gif"));
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10:duration=2"], &videos.join("anim.gif"));
    lossless_video(&ff, 1, "320x240", &videos.join(".hidden").join("secret.mp4"));
    std::fs::write(videos.join("old.batchviproc.w1.mp4"), b"leftover").unwrap();
    std::fs::copy(&big, videos.join("export.tmp.mp4")).unwrap();

    // Old timestamp + read-only, like files copied from a phone backup.
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800); // 2020-01-01
    std::fs::File::options().write(true).open(&big).unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old).set_accessed(old)).unwrap();
    let mut perms = std::fs::metadata(&big).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&big, perms).unwrap();
    let big_before = std::fs::metadata(&big).unwrap().len();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let s = engine.snapshot();
        assert_eq!(s.total_files, 7, "logs: {:#?}", rec.logs.lock().unwrap());
        assert_eq!(s.skipped_generated, 1);
        assert_eq!(s.pending_files, 6);
        assert!(!videos.join("old.batchviproc.w1.mp4").exists(), "orphan temp not removed");

        engine.clone().run_batch(opts("av1_nvenc", 2.0)).await;
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Done, "{logs}");
    assert_eq!(s.completed_this_run, 6, "{logs}");
    assert_eq!(s.compressed_this_run, 3, "{logs}"); // big.mp4, clip.mov, anim.gif
    assert_eq!(s.failed_this_run, 1, "{logs}"); // broken.mp4
    assert_eq!(s.skipped_larger_this_run, 2, "{logs}"); // voice.mp4, static.gif

    let big_after = std::fs::metadata(&big).unwrap();
    assert!(big_after.len() < big_before, "big.mp4 was not compressed");
    assert_eq!(big_after.modified().unwrap(), old, "modified date not preserved");
    assert!(!videos.join("sub").join("clip.mov").exists());
    assert!(videos.join("sub").join("clip.mp4").exists());
    assert!(videos.join("anim.mp4").exists() && !videos.join("anim.gif").exists());
    assert!(videos.join("static.gif").exists() && videos.join("voice.mp4").exists());
    assert!(videos.join(".hidden").join("secret.mp4").exists());
    let leftovers: Vec<_> = walkdir::WalkDir::new(&videos).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".batchviproc.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    // History persisted: a fresh engine on the same folder finds nothing new.
    let history = std::fs::read_to_string(root.join("appdata").join("processedFiles.txt")).unwrap();
    assert!(history.lines().count() >= 6);
    let (engine2, _) = new_engine(&rt, &root, ff);
    rt.block_on(engine2.load_dir(videos.clone()));
    assert_eq!(engine2.snapshot().pending_files, 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stop_kills_ffmpeg_and_keeps_originals() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("stop");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    let long = videos.join("long.mp4");
    lossless_video(&ff, 40, "1920x1080", &long);
    let size_before = std::fs::metadata(&long).unwrap().len();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 1.0)));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(engine.snapshot().status, Status::Running);
        engine.stop();
        tokio::time::timeout(Duration::from_secs(15), runner).await
            .expect("batch did not stop within 15s").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Ready, "{logs}");
    assert_eq!(std::fs::metadata(&long).unwrap().len(), size_before, "original changed");
    assert!(!videos.join("long.batchviproc.w1.mp4").exists(), "temp file left behind");
    assert!(logs.contains("Stopped while processing long.mp4"), "{logs}");

    let _ = std::fs::remove_dir_all(&root);
}

/// Audio codec of the first audio stream, read from ffmpeg's stream listing.
/// Waits until the single worker reports progress above 0%.
async fn wait_for_progress(engine: &Arc<Engine>) -> i64 {
    for _ in 0..200 {
        let pct = engine.snapshot().telemetry.workers.first().map_or(0, |w| w.percent);
        if pct > 0 {
            return pct;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("encoding never made progress");
}

/// Pause suspends ffmpeg (no progress, no output growth, no new file started); Resume
/// finishes the batch normally. Set BATCHVIPROC_PAUSE_SECS=70 to also prove that a long
/// pause does not trip the 30/60 s stall watchdog, and BATCHVIPROC_PAUSE_ENCODER=av1_nvenc
/// to pause a GPU encode.
#[test]
fn pause_suspends_ffmpeg_and_resume_finishes() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let pause_secs: u64 = std::env::var("BATCHVIPROC_PAUSE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let encoder = std::env::var("BATCHVIPROC_PAUSE_ENCODER").unwrap_or_else(|_| "libx264".into());
    let root = test_dir("pause");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    lossless_video(&ff, 12, "1280x720", &videos.join("a.mp4"));
    lossless_video(&ff, 4, "640x360", &videos.join("b.mp4"));
    let temp = videos.join("a.batchviproc.w1.mp4");

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(opts(&encoder, 1.0)));
        wait_for_progress(&engine).await;

        engine.set_paused(true);
        tokio::time::sleep(Duration::from_millis(700)).await; // let in-flight output settle
        let before = engine.snapshot();
        let pct_before = before.telemetry.workers[0].percent;
        let size_before = std::fs::metadata(&temp).map_or(0, |m| m.len());
        assert_eq!(before.status, Status::Paused);

        tokio::time::sleep(Duration::from_secs(pause_secs)).await;
        let after = engine.snapshot();
        assert_eq!(after.status, Status::Paused);
        assert_eq!(after.telemetry.workers[0].percent, pct_before, "progress moved while paused");
        assert_eq!(after.telemetry.total_fps, 0);
        assert_eq!(std::fs::metadata(&temp).map_or(0, |m| m.len()), size_before, "output grew while paused");
        assert_eq!(after.completed_this_run, 0);
        assert_eq!(after.current_file, "a.mp4", "a new file started while paused");

        engine.set_paused(false);
        assert_eq!(engine.snapshot().status, Status::Running);
        tokio::time::timeout(Duration::from_secs(120), runner).await
            .expect("batch did not finish after resume").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Done, "{logs}");
    assert_eq!(s.compressed_this_run + s.skipped_larger_this_run, 2, "{logs}");
    assert_eq!(s.failed_this_run, 0, "{logs}");
    assert!(logs.contains("Paused (1 ffmpeg process(es) suspended)"), "{logs}");
    assert!(logs.contains("Resumed"), "{logs}");
    assert!(!logs.contains("Stall watchdog"), "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Stop works while paused: the suspended ffmpeg is killed and originals are untouched.
#[test]
fn stop_while_paused_keeps_originals() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("pause-stop");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    let long = videos.join("long.mp4");
    lossless_video(&ff, 30, "1920x1080", &long);
    let size_before = std::fs::metadata(&long).unwrap().len();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 1.0)));
        wait_for_progress(&engine).await;
        engine.set_paused(true);
        tokio::time::sleep(Duration::from_secs(1)).await;
        engine.stop();
        tokio::time::timeout(Duration::from_secs(15), runner).await
            .expect("batch did not stop within 15s").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    assert_eq!(engine.snapshot().status, Status::Ready, "{logs}");
    assert!(!engine.is_paused());
    assert_eq!(std::fs::metadata(&long).unwrap().len(), size_before, "original changed");
    assert!(!videos.join("long.batchviproc.w1.mp4").exists(), "temp file left behind");
    let _ = std::fs::remove_dir_all(&root);
}

fn audio_codec(ffmpeg: &Path, file: &Path) -> Option<String> {
    let out = Command::new(ffmpeg).args(["-hide_banner", "-i"]).arg(file).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text.lines().find(|l| l.contains("Audio:"))?;
    let codec = line.split("Audio:").nth(1)?.split_whitespace().next()?;
    Some(codec.trim_end_matches(',').to_string())
}

/// Regression test for the real-world files from the project history ("old fixes"):
/// each of these once broke a batch.
#[test]
fn historical_problem_files_are_handled() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("history");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();

    // 1. Old game bonus video: WMV container with WMA audio (can't be stream-copied to MP4).
    make(
        &ff,
        &["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=4", "-f", "lavfi", "-i",
          "sine=duration=4", "-c:v", "wmv2", "-q:v", "2", "-c:a", "wmav2", "-b:a", "128k", "-shortest"],
        &videos.join("Developer Diary.wmv"),
    );
    // 2. Small animated sticker GIF (above the 16x16 skip threshold).
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=40x40:rate=10:duration=2"], &videos.join("sticker.gif"));
    // 3. "Troll" video that changes resolution mid-stream (two MPEG-TS segments glued together).
    let seg_a = root.join("a.ts");
    let seg_b = root.join("b.ts");
    for (seg, size) in [(&seg_a, "640x360"), (&seg_b, "320x240")] {
        let src = format!("testsrc2=size={size}:rate=30:duration=2");
        make(&ff, &["-f", "lavfi", "-i", &src, "-f", "lavfi", "-i", "sine=duration=2", "-c:v",
                    "libx264", "-preset", "ultrafast", "-qp", "0", "-c:a", "aac", "-shortest"], seg);
    }
    let mut troll = std::fs::read(&seg_a).unwrap();
    troll.extend(std::fs::read(&seg_b).unwrap());
    std::fs::write(videos.join("troll.ts"), troll).unwrap();
    // 4. Accented name with an upper-case extension.
    lossless_video(&ff, 2, "640x360", &videos.join("Vídeo Ação.MP4"));
    // 5. Remux target already exists: dup.mov would become dup.mp4, which is another file.
    lossless_video(&ff, 2, "640x360", &videos.join("dup.mov"));
    std::fs::write(videos.join("dup.mp4"), b"not really a video").unwrap();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        engine.clone().run_batch(opts("av1_nvenc", 2.0)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");

    // WMV: converted to MP4 with AAC audio, original removed.
    let wmv_out = videos.join("Developer Diary.mp4");
    assert!(wmv_out.exists() && !videos.join("Developer Diary.wmv").exists(), "{logs}");
    assert_eq!(audio_codec(&ff, &wmv_out).as_deref(), Some("aac"), "{logs}");
    // Sticker GIF: below NVENC's minimum size, so it must take the CPU fallback and finish
    // cleanly (converted, or kept when the MP4 isn't smaller), never fail.
    assert!(logs.contains("NVENC hardware limit on sticker.gif"), "{logs}");
    assert!(!logs.contains("exited with code") , "{logs}");
    assert!(videos.join("sticker.mp4").exists() != videos.join("sticker.gif").exists(), "{logs}");
    // The fake dup.mp4 is detected as unreadable and remembered, not retried forever.
    assert!(logs.contains("Corrupt/unreadable file remembered in history (will not retry): dup.mp4"), "{logs}");
    // Resolution change: encoded instead of failing.
    assert!(videos.join("troll.mp4").exists() && !videos.join("troll.ts").exists(), "{logs}");
    // Accented name: processed in place and remembered.
    assert!(videos.join("Vídeo Ação.MP4").exists(), "{logs}");
    assert!(logs.contains("Saved") && logs.contains("Vídeo Ação.MP4"), "{logs}");
    // Existing target: the original .mov is kept and the unrelated .mp4 untouched.
    assert!(videos.join("dup.mov").exists(), "{logs}");
    assert_eq!(std::fs::read(videos.join("dup.mp4")).unwrap(), b"not really a video");
    assert!(logs.contains("Target dup.mp4 already exists"), "{logs}");
    // No temp files left anywhere.
    let leftovers: Vec<_> = walkdir::WalkDir::new(&videos).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".batchviproc.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    // Everything is remembered: a second scan finds nothing new.
    let (engine2, _) = new_engine(&rt, &root, ff);
    rt.block_on(engine2.load_dir(videos.clone()));
    assert_eq!(engine2.snapshot().pending_files, 0, "{logs}");

    let _ = std::fs::remove_dir_all(&root);
}
