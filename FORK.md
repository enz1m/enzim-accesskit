# Fork provenance

- Upstream: https://github.com/AccessKit/accesskit
- Base: `c88605b96d04431f9c3c792464a0f2f253480e94`
- Branch: `enzim`; crate names and upstream ancestry are unchanged.
- License: Per-package upstream licenses. Upstream license files and notices are retained.
- Fork author and signer: Bernard, [enz1m](https://github.com/enz1m).

## Changes

- atspi: Keep adapter registrations sorted
- atspi: Withdraw all interfaces when an adapter retires
- atspi: Expose label and description relations
- atspi: Report busy nodes
- atspi: Expose editable text input operations
- atspi: Clear enabled state for disabled nodes
- unix: Add bounded virtual text providers
- unix: Match the zbus executor to the selected runtime
- unix: Expose stable AT-SPI window identity
- unix: Preserve virtual text refusal classes
- unix: Keep adapter message entries sorted
- unix: Encode cache signals as one struct argument
- unix: Format virtual text handlers
- unix: Support native editable text operations
- winit: Route adapter-scoped virtual text providers
- build: Pin the prepared zbus family and winit; update the standalone lockfile

The three source archives record the same upstream commit. `accesskit_winit`
remains Apache-2.0; the common and Unix packages retain MIT OR Apache-2.0.
`LICENSE.chromium` and all existing notices are preserved.

## Dependency routing

The standalone workspace uses root patches for winit and the matching zbus,
zbus_macros, zbus_names, zvariant, zvariant_derive and zvariant_utils sources.
These zbus crates must resolve together to preserve their shared public types. Consumers must copy the `[patch.crates-io]` entries from this repository's
Cargo.toml into their own workspace root to select the same sources. The Unix
adapter retains its upstream async-io default; products select Tokio explicitly.

Use the same Git source for the `accesskit` and `accesskit_consumer` crates
when consuming these adapters. Their public tree types must not be mixed with
registry copies. All three adapter crates and these two common crates can be
patched to this repository at one reviewed revision.
