use cocoa::appkit::{NSApp, NSApplication, NSPasteboard};
use cocoa::base::nil;
use cocoa::foundation::NSAutoreleasePool;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub struct Listener {
    notify: Arc<Notify>,
}

impl Listener {
    pub fn new() -> anyhow::Result<Self> {
        let notify = Arc::new(Notify::const_new());
        let dupe = Arc::clone(&notify);

        let poll_interval = match std::env::var("YCLIP_MACOS_POLL_INTERVAL_MILLIS") {
            Ok(s) => Duration::from_millis(s.trim().parse()?),
            Err(std::env::VarError::NotPresent) => Duration::from_millis(200),
            Err(e) => return Err(e.into()),
        };
        std::thread::spawn(|| listen_clipboard(dupe, poll_interval));

        Ok(Self { notify })
    }

    pub async fn change(&self) -> anyhow::Result<()> {
        Ok(self.notify.notified().await)
    }
}

pub fn listen_clipboard(
    notify: Arc<Notify>,
    poll_interval: Duration,
) -> Result<(), Box<dyn Error>> {
    unsafe {
        let _pool = NSAutoreleasePool::new(nil);
        let app = NSApp();
        app.setActivationPolicy_(cocoa::appkit::NSApplicationActivationPolicyProhibited);

        let pasteboard = NSPasteboard::generalPasteboard(nil);
        let mut count = pasteboard.changeCount();

        loop {
            let new_count = pasteboard.changeCount();
            if new_count != count {
                notify.notify_one();
                count = new_count;
            }
            std::thread::sleep(poll_interval);
        }
    }
}
