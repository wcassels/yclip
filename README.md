<h1 align="center">yclip</h1>

`yclip` implements a client-server model for sharing clipboard content. Invoke `yclip` to start a server locally; it will log a command in the form `yclip <host>:<port>` that you can run from elsewhere on the network to connect. Once connected, text/images copied on one machine will be instantly* pastable on the other!

Run `yclip --help` for the options: 
```
Usage: yclip [OPTIONS] [SOCKET]

Arguments:
  [SOCKET]  Connect to the yclip server running on this socket address

Options:
  -i, --poll-interval <POLL_INTERVAL>  Frequency in ms with which to actually poll for
                                       clipboard changes (see README) [default: 10000]
  -p, --password <PASSWORD>            Encrypt clipboards using this password. Compile with
                                       the "force-secure" feature enabled to make this
                                       mandatory
  -v, --verbose...                     Increase verbosity (defaults to INFO and above)
  -h, --help                           Print help
  -V, --version                        Print version

```

\* `yclip` is cross-platform. 

On Windows, we can subscribe to `WM_CLIPBOARDUPDATE` events which are triggered whenever the clipboard changes. Easy!

On MacOS we can't sign up to be notified on clipboard changes. Instead, we have to poll the `changeCount` on `NSPasteBoard` and check if it's increased since last time. This means there's actually work to do to detect changes in a timely manner; I've set the default poll interval to be 200ms but if you find this is using up too many CPU cycles then you can reduce the frequency via the `YCLIP_MACOS_POLL_INTERVAL_MILLIS` environment variable.

On X11 linux systems, the situation is similar to Windows; in this case we can subscribe to the relevant [XFIXES](https://www.x.org/releases/current/doc/fixesproto/fixesproto.txt) event, which is issued whenever someone asserts ownership of the clipboard. Sadly this isn't a catch-all; the current clipboard owner can change its contents without telling the X server about it (although this is against the ICCCM rules!) and so we'd miss it. As a safety measure, `yclip` polls the clipboard every so often (10s by default, adjust with the `-i` flag) and sees if it's changed.

On Wayland, uhhh... I have no idea how Wayland works.

## Installation

You can build `yclip` from source using cargo. Either install it with `cargo install yclip --git <this url>`, or checkout this repo and run  `cargo build --release` - you'll find the binary in `target/release/`. If you want to make password-encryption mandatory, append `--features force-secure` to either command.

## Encryption

`yclip` uses a ChaChaPoly1305 encryption scheme, with keys derived from the user-provided password via the NNpsk0 Noise protocol. Even without a password, network bytes will still be encrypted - it's just that anyone will be able to connect to your clipboard!

## Testing

I've implemented a fuzzer that simulates a client and a server communicating via an in-memory channel, testing that the clipboards are synced as expected. It hasn't failed yet! Run it yourself with `cargo +nightly fuzz run --release local` (this requires a nightly release of Cargo).
