//! OS integration: keeping the PC awake, tying ffmpeg children to the app's lifetime,
//! memory checks, single-instance locking and Windows-friendly file operations.
//!
//! Everything in here that touches the disk is blocking; async callers run it through
//! `spawn_blocking` so slow disks or network shares never stall the worker threads.

use std::fs::{self, File, FileTimes};
use std::io::Read;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

/// Prevents system sleep while alive (like Electron's 'prevent-app-suspension').
/// SetThreadExecutionState is per-thread, so a dedicated thread holds the request.
pub struct PowerGuard {
    release: Option<mpsc::Sender<()>>,
}

impl PowerGuard {
    pub fn acquire() -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::System::Power::{
                    SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
                };
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
                let _ = rx.recv();
                SetThreadExecutionState(ES_CONTINUOUS);
            }
            #[cfg(not(windows))]
            let _ = rx.recv();
        });
        Self { release: Some(tx) }
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(());
        }
    }
}

/// A Windows job object with KILL_ON_JOB_CLOSE: every ffmpeg process assigned to it is
/// terminated automatically when the app exits, even if it crashes.
#[cfg(windows)]
mod job {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    static JOB: OnceLock<usize> = OnceLock::new();

    fn handle() -> usize {
        *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            job as usize
        })
    }

    pub fn assign(process: std::os::windows::io::RawHandle) {
        let job = handle();
        if job != 0 && !process.is_null() {
            unsafe {
                AssignProcessToJobObject(job as _, process as _);
            }
        }
    }
}

pub fn assign_to_kill_on_exit_job(child: &tokio::process::Child) {
    #[cfg(windows)]
    if let Some(h) = child.raw_handle() {
        job::assign(h);
    }
    #[cfg(not(windows))]
    let _ = child;
}

#[cfg(windows)]
#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtSuspendProcess(process: *mut core::ffi::c_void) -> i32;
    fn NtResumeProcess(process: *mut core::ffi::c_void) -> i32;
}

/// Freezes (or thaws) every thread of a child process, which is how Pause works: ffmpeg
/// keeps its state and output file and simply continues where it was. Returns success.
pub fn set_process_suspended(child: &tokio::process::Child, suspended: bool) -> bool {
    #[cfg(windows)]
    if let Some(h) = child.raw_handle() {
        // SAFETY: `h` is the live process handle owned by `child` (full access rights).
        let status = unsafe {
            if suspended {
                NtSuspendProcess(h as _)
            } else {
                NtResumeProcess(h as _)
            }
        };
        return status >= 0;
    }
    let _ = (child, suspended);
    false
}

/// Hides the console window. Encoders run at below-normal priority: the desktop stays
/// responsive, but unlike idle priority the encode doesn't stall completely while
/// something else (a game, a build) keeps the CPU busy.
pub fn configure_background_command(cmd: &mut tokio::process::Command, low_priority: bool) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let flags = if low_priority {
            CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
        } else {
            CREATE_NO_WINDOW
        };
        cmd.creation_flags(flags);
    }
    #[cfg(not(windows))]
    let _ = (cmd, low_priority);
}

/// Free physical memory and free commit (RAM + page file) in bytes.
#[derive(Clone, Copy, Debug)]
pub struct MemoryStatus {
    pub available_physical: u64,
    pub available_commit: u64,
}

pub fn memory_status() -> Option<MemoryStatus> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return None;
        }
        Some(MemoryStatus {
            available_physical: status.ullAvailPhys,
            available_commit: status.ullAvailPageFile,
        })
    }
    #[cfg(not(windows))]
    None
}

/// Holds a named mutex so only one batchviproc runs at a time (two instances would
/// append to the same history file and fight over the same folders).
pub struct SingleInstance {
    #[cfg(windows)]
    _handle: usize,
}

impl SingleInstance {
    /// Returns None when another instance already holds the lock.
    pub fn acquire(name: &str) -> Option<Self> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
            use windows_sys::Win32::System::Threading::CreateMutexW;
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let handle = CreateMutexW(std::ptr::null(), 0, wide.as_ptr());
            if handle.is_null() {
                // Can't tell; don't block the user.
                return Some(Self { _handle: 0 });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return None;
            }
            // Intentionally kept open for the life of the process.
            Some(Self { _handle: handle as usize })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Some(Self {})
        }
    }
}

fn clear_readonly(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
}

/// Deletes a file if present, clearing the read-only attribute first.
pub fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    clear_readonly(path);
    fs::remove_file(path)
}

/// Replaces `final_target` with `tmp`, coping with read-only originals (common on phone
/// backups) and Explorer or antivirus briefly holding a handle. Removes `original` if the
/// name changed. Blocking: sleeps between retries.
pub fn replace_file_with_retry(tmp: &Path, final_target: &Path, original: &Path) -> std::io::Result<()> {
    const MAX_ATTEMPTS: u64 = 6;
    let mut attempt = 1;
    loop {
        let result = (|| -> std::io::Result<()> {
            clear_readonly(original);
            if final_target != original {
                clear_readonly(final_target);
            }
            if let Err(err) = fs::rename(tmp, final_target) {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    fs::copy(tmp, final_target)?;
                    fs::remove_file(tmp)?;
                } else {
                    return Err(err);
                }
            }
            if final_target != original {
                let _ = remove_file_if_exists(original);
            }
            Ok(())
        })();

        match result {
            Ok(()) => return Ok(()),
            Err(err) if attempt >= MAX_ATTEMPTS => return Err(err),
            Err(_) => {
                std::thread::sleep(Duration::from_millis(250 * attempt));
                attempt += 1;
            }
        }
    }
}

/// Dates and the read-only flag of the original file, restored on the recompressed copy.
pub struct OriginalAttributes {
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub readonly: bool,
}

impl OriginalAttributes {
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            created: meta.created().ok(),
            accessed: meta.accessed().ok(),
            modified: meta.modified().ok(),
            readonly: meta.permissions().readonly(),
        }
    }
}

/// Restores creation, access and modification dates, then the read-only flag.
pub fn restore_attributes(path: &Path, attrs: &OriginalAttributes) -> std::io::Result<()> {
    clear_readonly(path);
    {
        let file = File::options().write(true).open(path)?;
        let mut ft = FileTimes::new();
        if let Some(a) = attrs.accessed {
            ft = ft.set_accessed(a);
        }
        if let Some(m) = attrs.modified {
            ft = ft.set_modified(m);
        }
        #[cfg(windows)]
        if let Some(c) = attrs.created {
            use std::os::windows::fs::FileTimesExt;
            ft = ft.set_created(c);
        }
        file.set_times(ft)?;
    }
    if attrs.readonly {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn local_time() -> (u16, u16, u16, u16, u16, u16) {
    #[cfg(windows)]
    unsafe {
        let mut t = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        let t: windows_sys::Win32::Foundation::SYSTEMTIME = t;
        (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
    }
    #[cfg(not(windows))]
    {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (1970, 1, 1, (secs / 3600 % 24) as u16, (secs / 60 % 60) as u16, (secs % 60) as u16)
    }
}

/// Local wall-clock time as HH:MM:SS, for log lines.
pub fn local_time_hms() -> String {
    let (_, _, _, h, m, s) = local_time();
    format!("{h:02}:{m:02}:{s:02}")
}

/// Local date and time as YYYY-MM-DD HH:MM:SS, for log files.
pub fn local_datetime() -> String {
    let (y, mo, d, h, mi, s) = local_time();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Compact local timestamp for file names: YYYYMMDD-HHMMSS.
pub fn local_timestamp_compact() -> String {
    let (y, mo, d, h, mi, s) = local_time();
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Returns true only for real animated GIFs: at least 16x16 and more than one frame.
/// Static 1-frame GIFs and spacer GIFs (DOT.GIF, spacer.gif) return false.
/// Reads in chunks and stops at the second frame marker, so large GIFs aren't loaded whole.
/// Graphic Control Extension block header in a GIF stream.
const GIF_GCE_MARKER: [u8; 3] = [0x21, 0xF9, 0x04];

pub fn is_animated_and_valid_gif(mut reader: impl Read) -> bool {
    let mut header = [0u8; 13];
    if reader.read_exact(&mut header).is_err()
        || !(header.starts_with(b"GIF89a") || header.starts_with(b"GIF87a"))
    {
        return false;
    }
    let width = u16::from_le_bytes([header[6], header[7]]);
    let height = u16::from_le_bytes([header[8], header[9]]);
    if width < 16 || height < 16 {
        return false;
    }

    // Count Graphic Control Extension blocks: more than one means animated.
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry = 0usize; // bytes kept from the previous chunk so markers can span chunks
    let mut found = 0;
    loop {
        let n = match reader.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let filled = carry + n;
        let window = &buf[..filled];
        let mut i = 0;
        while i + 3 <= filled {
            if window[i..i + 3] == GIF_GCE_MARKER {
                found += 1;
                if found >= 2 {
                    return true;
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        // Keep the last two bytes (a marker can't start later than that).
        carry = filled.min(2);
        buf.copy_within(filled - carry..filled, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn gif(w: u16, h: u16, frames: usize, padding_between: usize) -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend(w.to_le_bytes());
        g.extend(h.to_le_bytes());
        g.extend([0u8; 3]);
        g.extend(vec![0u8; 20]);
        for _ in 0..frames {
            g.extend(vec![0u8; padding_between]);
            g.extend([0x21, 0xF9, 0x04]);
        }
        g
    }

    #[test]
    fn gif_detection() {
        assert!(!is_animated_and_valid_gif(Cursor::new(gif(32, 32, 1, 10))));
        assert!(is_animated_and_valid_gif(Cursor::new(gif(32, 32, 2, 10))));
        assert!(!is_animated_and_valid_gif(Cursor::new(gif(1, 1, 5, 10)))); // spacer
        assert!(!is_animated_and_valid_gif(Cursor::new(b"not a gif at all".to_vec())));
    }

    #[test]
    fn gif_markers_split_across_chunks_are_found() {
        // Second marker straddles the 64 KiB chunk boundary.
        let mut g = gif(64, 64, 1, 0);
        let pad = 64 * 1024 - (g.len() - 13) - 1;
        g.extend(vec![0u8; pad]);
        g.extend([0x21, 0xF9, 0x04]);
        assert!(is_animated_and_valid_gif(Cursor::new(g)));
    }

    #[test]
    fn restores_dates_and_readonly_flag() {
        let path = std::env::temp_dir().join(format!("bvp-attrs-{}.bin", std::process::id()));
        std::fs::write(&path, b"data").unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
        let attrs = OriginalAttributes {
            created: Some(old),
            accessed: Some(old),
            modified: Some(old),
            readonly: true,
        };
        restore_attributes(&path, &attrs).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.modified().unwrap(), old);
        assert!(meta.permissions().readonly());
        remove_file_if_exists(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn single_instance_lock_is_exclusive() {
        let name = format!("Local\\batchviproc-test-{}", std::process::id());
        let first = SingleInstance::acquire(&name);
        assert!(first.is_some());
        assert!(SingleInstance::acquire(&name).is_none());
    }
}
