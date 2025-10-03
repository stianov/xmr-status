# xmr-status
A simple status page for Monero nodes. It has live updates through ZMQ, and shows the newest mempool transactions received.

If your daemon requires authentication, start it with `--rpc-login <user>:<password>` and supply the same credentials to the status server.

## Getting started
`cargo run --release`

Run the app for the first time to generate a `config.toml` file and fill in the fields. You can also override the config file with environment variables, but this is legacy and will be removed and replaced with command line parameters soon.

Your node must have ZMQ enabled. Here is an example command to run monerod:

```bash
./monerod --zmq-pub tcp://127.0.0.1:18083 --rpc-bind-port 18081 
```

## Debug
To toggle logging, set the `RUST_LOG` environment variable using standard syntax.

```bash
RUST_LOG="xmr_status=debug" cargo run
```