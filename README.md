# yclip

`yclip` implements a client-server model for sharing clipboard text. Invoking `yclip` starts a server locally; it'll log a command in the form `yclip <socket addr>` that you can run to connect from another machine on the network. Note that `yclip` understands hostnames too, as well as IPs.

Run `yclip --help` for the options: 
```
Usage: yclip [OPTIONS] [SOCKET]

Arguments:
  [SOCKET]  Connect to the yclip server running on this socket address

Options:
  -r, --refresh-interval <REFRESH_INTERVAL>
          Local clipboard check interval (ms) [default: 200]
  -s, --secret <SECRET>
          Encrypt clipboards using this secret. Compile with the "force-secure" feature
          enabled to make encryption mandatory
  -v, --verbose...
          Increase verbosity (defaults to INFO and above)
  -h, --help
          Print help
  -V, --version
          Print version
```

## Compiling

`yclip` is a rust project; build it with `cargo build --release` (adding `--features force-secure` to make encryption mandatory).

## Encryption

`yclip` uses a ChaChaPoly1305 encryption scheme, with keys derived from the user-provided password via the NNpsk0 Noise protocol.

## Fuzzing

I've implemented a fuzzer that simulates a client and a server communicating via an in-memory channel. To test it out, run `cargo +nightly fuzz run --release local` (this requires a nightly release of Cargo).
