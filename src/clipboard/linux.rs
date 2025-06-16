use std::os::fd::AsRawFd as _;
use tokio::io::unix::AsyncFd;
use tracing::*;
use x11rb::{
    connection::Connection as _,
    protocol::{
        self,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

pub struct Listener {
    x_conn: RustConnection,
}

impl Listener {
    pub fn new() -> anyhow::Result<Self> {
        let (x_conn, screen_num) = RustConnection::connect(None)?;
        let screen = &x_conn.setup().roots[screen_num];

        // Initialize the XFixes extension
        let xfixes_query = x_conn.xfixes_query_version(5, 0)?.reply()?;
        trace!(
            "XFIXES version: {}.{}",
            xfixes_query.major_version,
            xfixes_query.minor_version
        );

        // Select for SelectionNotify events
        let atom = x_conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        x_conn.xfixes_select_selection_input(
            screen.root,
            atom,
            SelectionEventMask::SET_SELECTION_OWNER,
        )?;

        x_conn.flush()?;

        Ok(Self { x_conn })
    }

    pub async fn change(&self) -> anyhow::Result<()> {
        async {
            let async_fd = AsyncFd::new(self.x_conn.stream().as_raw_fd())?;
            trace!("Listening for SelectionNotify events...");

            loop {
                let mut guard = async_fd.readable().await?;

                match self.x_conn.poll_for_event()? {
                    Some(protocol::Event::XfixesSelectionNotify(ev)) => {
                        debug!(
                            "Received SelectionNotify for selection atom: {}",
                            ev.selection
                        );
                        return Ok(());
                    }
                    Some(_) | None => {
                        // No event, clear readiness and wait again
                        guard.clear_ready();
                    }
                }
            }
        }
        .await
    }
}
