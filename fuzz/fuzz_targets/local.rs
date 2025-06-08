#![no_main]
use std::{
    sync::{atomic::AtomicUsize, atomic::Ordering, Arc, LazyLock, RwLock},
    time::Duration,
};
use tokio::{io::DuplexStream, runtime::Runtime, sync::Notify};
use yclip::{tracing::*, Clipboard, Noise, Secret};

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| Runtime::new().unwrap());
static NOTIFY_A: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::const_new()));
static NOTIFY_B: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::const_new()));

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
        let res = Ok(CLIPBOARD_B.read().unwrap().clone());
        debug!("A contents were {res:?}");
        res
    }
}

static COUNT: AtomicUsize = AtomicUsize::new(0);

async fn dummy_connections(
    password: &str,
) -> (
    yclip::Connection<DuplexStream>,
    yclip::Connection<DuplexStream>,
) {
    let (mut stream_a, mut stream_b) = tokio::io::duplex(65535);
    let secret = Secret::new(password, None);
    let (host, client) = tokio::join! {
        Noise::host(&mut stream_a, "b", &secret),
        Noise::satellite(&mut stream_b, password),
    };
    let host = host.unwrap().unwrap();
    let client = client.unwrap().unwrap();
    let connection_a = yclip::Connection::new(stream_a, "b", host);
    let connection_b = yclip::Connection::new(stream_b, "a", client);
    (connection_a, connection_b)
}

fn test(input: &str, password: &str) {
    let prev = ClipboardB.get_text().unwrap();

    RUNTIME.block_on(async {
        let (conn_a, conn_b) = dummy_connections(password).await;
        let handle_a = tokio::task::spawn(async {
            yclip::watch_remote::<_, ClipboardA>(conn_a, Arc::clone(&*NOTIFY_A))
                .await
                .unwrap();
        });
        let handle_b = tokio::task::spawn(async {
            yclip::watch_remote::<_, ClipboardB>(conn_b, Arc::clone(&*NOTIFY_B))
                .await
                .unwrap();
        });
        // Give the two tasks above time to register their notify watches.
        // Unfortunately just yielding doesn't guarantee the executor won't
        // immediately reschedule this task
        tokio::time::sleep(Duration::from_millis(1)).await;

        let notified = NOTIFY_B.notified();
        ClipboardA.set_text(input);

        // Empty input or same as before => don't expect B to change
        let change_expected = prev.as_deref() != Some(input) && !input.is_empty();
        let expected = if !change_expected {
            prev.as_deref()
        } else {
            // Just makes sure deadlocking/missing clipboard updates causes the
            // test case to fail
            tokio::time::timeout(Duration::from_secs(5), notified)
                .await
                .expect("Waited 5 seconds but didn't see the clipboard come through?");
            Some(input)
        };

        assert_eq!(
            ClipboardB.get_text().unwrap().as_deref(),
            expected,
            "input: {input}, password: {password}, change expected: {change_expected}. {} successful tests in total",
            COUNT.load(Ordering::Relaxed)
        );

        handle_a.abort();
        handle_b.abort();
    });

    let done = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if done % 10 == 0 {
        info!("Passed {done} tests (latest in: {input} pw: {password})");
    }
}

libfuzzer_sys::fuzz_target!(
    init: {
        yclip::init_logging(Level::INFO).unwrap();

        let notify_a = Arc::clone(&*NOTIFY_A);
        RUNTIME.spawn(async {
            yclip::watch_local::<ClipboardA>(notify_a, Duration::from_millis(2)).await.unwrap();
        });
    },
    |input: (String, String)| {
        let (input, password) = input;
        test(input.as_str(), password.as_str());
    }
);
