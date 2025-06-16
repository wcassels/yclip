use crate::{clipboard::Board, Connection, Noise, Secret};
use arboard::ImageData;
use rand::distr::{SampleString, Uniform};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, LazyLock, RwLock,
    },
    time::Duration,
};
use tokio::{io::DuplexStream, runtime::Runtime, sync::Notify};
use tracing::*;

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| Runtime::new().unwrap());
static NOTIFY_A: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::const_new()));
static NOTIFY_B: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::const_new()));

// Represent system clipboards on the respective machines
static CLIPBOARD_A: RwLock<Option<String>> = RwLock::new(None);
static CLIPBOARD_B: RwLock<Option<String>> = RwLock::new(None);

// And the API by which we access them
struct ClipboardA;
struct ClipboardB;

impl Board for ClipboardA {
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
    fn set_image(&mut self, _image: ImageData<'_>) {
        unimplemented!()
    }
    fn get_image(&mut self) -> anyhow::Result<Option<ImageData<'static>>> {
        Ok(None)
    }
}

impl Board for ClipboardB {
    fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }
    fn set_text(&mut self, text: &str) {
        let new = (!text.is_empty()).then(|| text.to_owned());
        *CLIPBOARD_B.write().unwrap() = new;
    }
    fn get_text(&mut self) -> anyhow::Result<Option<String>> {
        let res = Ok(CLIPBOARD_B.read().unwrap().clone());
        res
    }
    fn set_image(&mut self, _image: ImageData<'_>) {
        unimplemented!()
    }
    fn get_image(&mut self) -> anyhow::Result<Option<ImageData<'static>>> {
        Ok(None)
    }
}

static COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn test_setup() {
    let notify_a = Arc::clone(&*NOTIFY_A);
    RUNTIME.spawn(async {
        crate::watch_local::<ClipboardA>(notify_a, Duration::from_millis(2))
            .await
            .unwrap();
    });
}

pub fn test(input: &[String], password: &str) {
    RUNTIME.block_on(async {
        let (conn_a, conn_b) = dummy_connections(password).await;
        let handle_a = tokio::task::spawn(async {
            crate::watch_remote(conn_a, ClipboardA::new().unwrap(), Arc::clone(&*NOTIFY_A))
                .await
                .unwrap();
        });
        let handle_b = tokio::task::spawn(async {
            crate::watch_remote(conn_b, ClipboardB::new().unwrap(), Arc::clone(&*NOTIFY_B))
                .await
                .unwrap();
        });
        // Give the two tasks above time to register their notify watches.
        // Unfortunately just yielding doesn't guarantee the executor won't
        // immediately reschedule this task
        tokio::time::sleep(Duration::from_millis(1)).await;

        for x in input {
            let prev = ClipboardB.get_text().unwrap();

            let notified = NOTIFY_B.notified();
            ClipboardA.set_text(x.as_str());

            // Empty input or same as before => don't expect B to change
            let change_expected = prev.as_deref() != Some(x.as_str()) && !x.is_empty();
            let expected = if !change_expected {
                prev.as_deref()
            } else {
                // Just makes sure deadlocking/missing clipboard updates causes the
                // test case to fail
                tokio::time::timeout(Duration::from_secs(5), notified)
                    .await
                    .expect("Waited 5 seconds but didn't see the clipboard come through?");
                Some(x.as_str())
            };

            let b_contents = ClipboardB.get_text().unwrap();
            if std::cmp::min(b_contents.unwrap_or_default().len(), expected.unwrap_or_default().len()) <= 1000 {
                assert_eq!(
                    ClipboardB.get_text().unwrap().as_deref(),
                    expected,
                    "input: {}, password: {password}, change expected: {change_expected}. {} successful tests in total",
                    if x.len() < 100 { x }  else { "<long, omitting>" },
                    COUNT.load(Ordering::Relaxed)
                );
            } else if ClipboardB.get_text().unwrap().as_deref() != expected {
                panic!("ClipboardB's contents didn't match expected. Omitting contents due to length");
            }

        }

        handle_a.abort();
        handle_b.abort();
    });

    let done = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if done % 10 == 0 {
        info!("Passed {done} tests (latest in: {input:?} pw: {password})");
    }
}

async fn dummy_connections(password: &str) -> (Connection<DuplexStream>, Connection<DuplexStream>) {
    let (mut stream_a, mut stream_b) = tokio::io::duplex(65535);
    let secret = Secret::new(password, None);
    let (host, client) = tokio::join! {
        Noise::host(&mut stream_a, "b", &secret),
        Noise::satellite(&mut stream_b, password),
    };
    let host = host.unwrap();
    let client = client.unwrap();
    let connection_a = Connection::new(stream_a, "b", host);
    let connection_b = Connection::new(stream_b, "a", client);
    (connection_a, connection_b)
}

#[test]
fn test_chunking() {
    crate::init_logging(Level::TRACE).unwrap();
    let mut rng = rand::rng();
    let uniform = Uniform::<char>::new('\u{0020}', '\u{10FFFF}').unwrap();
    let mut clipboard = String::new();

    // Around 20MB (zstd gets it down to 12ish)
    uniform.append_string(&mut rng, &mut clipboard, 5_000_000);

    test_setup();
    test(&[clipboard], "foobar");

    // Now send more chunked clipboards
    let mut clipboard = String::new();
    let mut inputs = Vec::new();
    for _ in 0..50 {
        uniform.append_string(&mut rng, &mut clipboard, 500_000);
        inputs.push(clipboard.clone());
        clipboard.clear();
    }

    test(inputs.as_slice(), "foobar");
}
