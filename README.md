# yclip

`yclip` implements a client-server model for sharing clipboard text. Invoke `yclip` to start a server locally; it will log a command in the form `yclip <host>:<port>` that you can run from elsewhere on the network to connect. Once connected, text copied on one machine will be instantly pastable on the other!

Run `yclip --help` for the options: 
```
Usage: yclip [OPTIONS] [SOCKET]

Arguments:
  [SOCKET]  Connect to the yclip server running on this socket address

Options:
  -r, --refresh-interval <REFRESH_INTERVAL>
          Local clipboard check interval (ms) [default: 200]
  -p, --password <PASSWORD>
          Encrypt clipboards using this password. Compile with the "force-secure"
          feature enabled to make this mandatory
  -v, --verbose...
          Increase verbosity (defaults to INFO and above)
  -h, --help
          Print help
  -V, --version
          Print version
```

## Installation

You can install `yclip` with `cargo install yclip --git <this url>`. Alternatively, checkout this repo and build from source with `cargo build --release` - you'll find the binary in `target/release/`. If you want to make password-encryption mandatory, append `--features force-secure` to either command.

## Encryption

`yclip` uses a ChaChaPoly1305 encryption scheme, with keys derived from the user-provided password via the NNpsk0 Noise protocol. Even without a password, network bytes will still be encrypted - it's just that anyone will be able to connect to your clipboard!

## Testing

I've implemented a fuzzer that simulates a client and a server communicating via an in-memory channel, testing that the clipboards are synced as expected. It hasn't failed yet! Run it yourself with `cargo +nightly fuzz run --release local` (this requires a nightly release of Cargo).
