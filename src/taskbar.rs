//! Windows taskbar progress (the green/red bar on the app's taskbar button).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskbarProgress {
    None,
    Normal(u64),
    Paused(u64),
    Error,
}

#[cfg(windows)]
pub struct Taskbar {
    list: windows::Win32::UI::Shell::ITaskbarList3,
    hwnd: windows::Win32::Foundation::HWND,
    last: Option<TaskbarProgress>,
}

#[cfg(windows)]
impl Taskbar {
    pub fn new(hwnd: isize) -> Option<Self> {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
        unsafe {
            // COM may already be initialized on the UI thread; either way is fine.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let list: ITaskbarList3 =
                CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok()?;
            list.HrInit().ok()?;
            Some(Self {
                list,
                hwnd: windows::Win32::Foundation::HWND(hwnd as *mut _),
                last: None,
            })
        }
    }

    pub fn set(&mut self, progress: TaskbarProgress) {
        use windows::Win32::UI::Shell::{TBPF_ERROR, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED};
        if self.last == Some(progress) {
            return;
        }
        self.last = Some(progress);
        unsafe {
            let _ = match progress {
                TaskbarProgress::None => self.list.SetProgressState(self.hwnd, TBPF_NOPROGRESS),
                TaskbarProgress::Normal(pct) => self
                    .list
                    .SetProgressState(self.hwnd, TBPF_NORMAL)
                    .and_then(|_| self.list.SetProgressValue(self.hwnd, pct, 100)),
                TaskbarProgress::Paused(pct) => self
                    .list
                    .SetProgressState(self.hwnd, TBPF_PAUSED)
                    .and_then(|_| self.list.SetProgressValue(self.hwnd, pct, 100)),
                TaskbarProgress::Error => self
                    .list
                    .SetProgressState(self.hwnd, TBPF_ERROR)
                    .and_then(|_| self.list.SetProgressValue(self.hwnd, 100, 100)),
            };
        }
    }
}

#[cfg(not(windows))]
pub struct Taskbar;

#[cfg(not(windows))]
impl Taskbar {
    pub fn new(_hwnd: isize) -> Option<Self> {
        None
    }
    pub fn set(&mut self, _progress: TaskbarProgress) {}
}
