# Validation

Linux x86_64, Rust 1.95.0. All three adapter sources retain their installed
versions and match the existing 42 Rust files after modification notices.

- Six common-layer adapter registry tests pass.
- Six Unix virtual-text tests pass under Tokio, including real peer-to-peer
  D-Bus reads, edits, cancellation and error classification.
- The same six tests pass under the default async-io runtime.
- The winit bridge builds with Tokio and both Linux windowing backends.
- Cargo metadata contains one instance of every AccessKit crate, winit, zbus,
  and each selected zbus support crate, all at the intended source.
- The standalone Cargo.lock records the prepared dependency pins. Consumers
  must repeat root patches as described in FORK.md.

```sh
cargo +1.95.0 test --locked -p accesskit_atspi_common --lib adapter_registry
cargo +1.95.0 test --locked -p accesskit_unix --no-default-features --features tokio --lib virtual_text
cargo +1.95.0 test --locked -p accesskit_unix --lib virtual_text
cargo +1.95.0 check --locked -p accesskit_winit --no-default-features --features accesskit_unix,tokio,rwh_06,winit/wayland,winit/x11
```

Upstream license files, Chromium notices, ancestry and all fork signatures are
checked for publication. Full desktop/screen-reader and non-Linux testing were
not performed for this source preview. Existing applications are not repinned.
