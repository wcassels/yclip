#![no_main]
use std::{
    sync::{atomic::AtomicUsize, atomic::Ordering, Arc, LazyLock, RwLock},
    time::Duration,
};
use tokio::{runtime::Runtime, sync::Notify};
use yclip::Clipboard;

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| Runtime::new().unwrap());

// Represent system clipboards on the respective machines
static CLIPBOARD_A: RwLock<Option<String>> = RwLock::new(None);
static CLIPBOARD_B: RwLock<Option<String>> = RwLock::new(None);

// And the API by which we access them
struct ClipboardA;
struct ClipboardB;

impl yclip::Clipboard for ClipboardA {
    fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }
    fn set_text(&mut self, text: &str) {
        let new = (!text.is_empty()).then(|| text.to_owned());
        *CLIPBOARD_A.write().unwrap() = new;
    }
    fn get_text(&mut self) -> anyhow::Result<Option<String>> {
        Ok(CLIPBOARD_A.read().unwrap().clone())
    }
}

impl Clipboard for ClipboardB {
    fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }
    fn set_text(&mut self, text: &str) {
        let new = (!text.is_empty()).then(|| text.to_owned());
        *CLIPBOARD_B.write().unwrap() = new;
    }
    fn get_text(&mut self) -> anyhow::Result<Option<String>> {
        Ok(CLIPBOARD_B.read().unwrap().clone())
    }
}

static COUNT: AtomicUsize = AtomicUsize::new(0);

libfuzzer_sys::fuzz_target!(
    init: {
        let notify_a = Arc::new(Notify::const_new());
        let notify_b = Arc::new(Notify::const_new());
        let (stream_a, stream_b) = tokio::io::duplex(65535);
        // Note the second argument is peer addr!
        let connection_a = yclip::Connection::new(stream_a, String::from("b"), None);
        let connection_b = yclip::Connection::new(stream_b, String::from("a"), None);
        {
            let notify_a = Arc::clone(&notify_a);
            RUNTIME.spawn(async {
                yclip::watch_local::<ClipboardA>(notify_a, Duration::from_millis(2)).await.unwrap();
            });
        }
        RUNTIME.spawn(async {
            yclip::watch_remote::<_, ClipboardA>(connection_a, notify_a).await.unwrap();
        });
        RUNTIME.spawn(async {
            yclip::watch_remote::<_, ClipboardB>(connection_b, notify_b).await.unwrap();
        });
    },
    |input: String| {
        RUNTIME.block_on(async {
            let prev_b = ClipboardB.get_text().unwrap();
            ClipboardA.set_text(input.as_str());
            tokio::time::sleep(Duration::from_millis(6)).await;
            let expected_b = if input.is_empty() {
                // We don't transmit empty clipboards.
                prev_b
            } else {
                Some(input)
            };
            assert_eq!(ClipboardB.get_text().unwrap(), expected_b, "{} successful tests", COUNT.load(Ordering::Relaxed));
        });
        COUNT.fetch_add(1, Ordering::Relaxed);
    }
);
