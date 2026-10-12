# Local verification

Verified on **12 October 2026**, macOS arm64, Rust 1.99.0 and OpenSSH 10.0p2.

The local build used a workspace-installed Rust toolchain and Zig linker because this Mac did not have Xcode Command Line Tools. Those tools and their absolute paths are not part of this repository. Standard Rust build tools are used in CI and the documented installation.

## Checks

- `cargo fmt --check`
- `cargo test --locked`: 9 unit tests and 5 CLI integration tests.
- `cargo clippy --locked --all-targets -- -D warnings`
- `cargo build --locked --release`
- `python3 tests/e2e.py`: real OpenSSH against a temporary Go SSH server.
- Bash and Zsh completion script syntax checks.

Real SSH scenarios include all four forwarding modes, Unix sockets in both directions, 256 KiB binary roundtrips and half-close, concurrent clients, group/union selection, removed-config entries, bind errors, unknown host keys, initial authentication/connection errors, reconnect after disconnect, reconnect timeout, simultaneous startup, malformed IPC, SIGTERM cleanup and explicit shutdown.

The test server generates keys at runtime and allows only loopback targets and Unix paths within its temporary directory. No live remote server, personal SSH config or committed private key is used.

Linux, minimum Rust version and GitHub Actions status are separate checks in CI; this local record does not claim they ran on this Mac.
