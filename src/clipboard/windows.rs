use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::*;
use winapi::shared::minwindef::LPVOID;
use winapi::um::winuser::{
    AddClipboardFormatListener, CreateWindowExW, GetMessageW, RemoveClipboardFormatListener,
    CW_USEDEFAULT, MSG, WM_CLIPBOARDUPDATE, WS_OVERLAPPEDWINDOW,
};

pub struct Listener {
    notify: Arc<Notify>,
}

impl Listener {
    pub fn new() -> anyhow::Result<Self> {
        let notify = Arc::new(Notify::const_new());
        let dupe = Arc::clone(&notify);
        std::thread::spawn(|| listen_clipboard(dupe));
        Ok(Self { notify })
    }

    pub async fn change(&self) -> anyhow::Result<()> {
        Ok(self.notify.notified().await)
    }
}

fn listen_clipboard(notify: Arc<Notify>) -> anyhow::Result<()> {
    let class_name: Vec<u16> = OsStr::new("STATIC").encode_wide().chain(Some(0)).collect();

    unsafe {
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            ptr::null(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut() as LPVOID,
        );

        anyhow::ensure!(!hwnd.is_null(), "Failed to create window");
        anyhow::ensure!(
            AddClipboardFormatListener(hwnd) != 0,
            "Failed to add clipboard format listener"
        );

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let ret = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
            debug!("GetMessageW returned {ret}");
            if ret > 0 && msg.message == WM_CLIPBOARDUPDATE {
                notify.notify_one();
            }
        }
        // while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
        //     if msg.message == WM_CLIPBOARDUPDATE {
        //         notify.notify_one();
        //     }
        //     // winapi::um::winuser::TranslateMessage(&msg);
        //     // winapi::um::winuser::DispatchMessageW(&msg);
        // }

        // RemoveClipboardFormatListener(hwnd);
    }
    // Ok(())
}
