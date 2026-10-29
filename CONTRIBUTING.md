# Contributing

Use the toolchain in `rust-toolchain.toml` and commit `Cargo.lock` changes deliberately.
See the [packaging guide](PACKAGING.md) for native prerequisites.
Tests use local fixtures and fake services; do not add account credentials or real-session
RTP dumps. WebRTC integration tests require local UDP sockets, not an Xbox login.

## Checks

```sh
bash scripts/generate-test-fixture.sh
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo build --locked --release -p xcloud-app --all-features
```

`cargo test` alone selects the desktop package; use `--workspace` to check every crate.
GPU tests may skip when no suitable adapter is available. Keychain tests and performance
benchmarks marked ignored are opt-in; a passing headless suite is not a live-stream test.
Use `cargo run --locked -p xcloud-app -- --smoke` on a machine with a desktop session
for the UI smoke run. It uses detached snapshots and does not sign in.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `xcloud-app` | Desktop shell, library, settings, and stream UI |
| `xcloud-api` | Authentication, credential storage, catalog, and session HTTP APIs |
| `xcloud-proto` | Protocol models, SDP, channels, input packets, and jitter buffers |
| `xcloud-webrtc` | webrtc-rs worker, transport lifecycle, RTP and RTCP |
| `xcloud-stream` | Session orchestration and media pipeline |
| `xcloud-media` | Decode, frame mailbox, and audio output |
| `xcloud-gpu` | Video import, colour conversion, spatial upscaling, and presentation |
| `xcloud-input` | Controller and keyboard input |

Keep protocol byte layouts covered by regression tests. Keep platform code behind
`cfg` boundaries and avoid blocking work on the UI or transport threads. Never log
tokens, authorization headers, device codes, or account data. Preserve source attribution
when changing ported code. The historical Qt/Swift source references are provenance,
not required local directories for building this repository.

The workspace is a desktop product; its internal crates are not published to crates.io.
