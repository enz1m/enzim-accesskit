# enzim-accesskit

An independently maintained enzUI fork of [accesskit](https://github.com/AccessKit/accesskit).

Ordered AT-SPI adapter lifecycle, text providers, editable text and the winit provider bridge.

The canonical repository is [enz1m/enzim-accesskit](https://github.com/enz1m/enzim-accesskit).
Upstream crate names, history and license notices are retained.

```sh
git clone --branch enzim https://github.com/enz1m/enzim-accesskit.git
cd enzim-accesskit
```

See [FORK.md](FORK.md) for the base and patch list,
[VALIDATION.md](VALIDATION.md) for checks. The
[upstream README](README.upstream.md) is retained.

The three patched crates stay in upstream order: `accesskit_atspi_common`,
`accesskit_unix`, then `accesskit_winit`. The workspace pins the prepared zbus
and winit forks. Cargo consumers must repeat the root patches documented in
[FORK.md](FORK.md); dependency patch tables do not propagate.
