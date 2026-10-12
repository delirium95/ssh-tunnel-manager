# Contributing

Use Rust 1.88+ and OpenSSH 8.0+. Run formatting, Clippy and the test suite before submitting a change:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
(cd tests/fixture && go build -o ../../target/ssh-fixture .)
python3 tests/e2e.py
```

The fixture requires Go 1.27+ and Python 3.10+. Keep tests isolated from personal SSH keys, configuration and remote services. Add a regression scenario for lifecycle or forwarding changes. Preserve the bounded IPC protocol and private runtime directory requirements.

Useful next contributions include native Windows IPC/process supervision, an interactive status view and persistent desired state across reboots. Security reports can be submitted through GitHub's private vulnerability reporting when enabled; avoid posting credentials or private keys in issues.
