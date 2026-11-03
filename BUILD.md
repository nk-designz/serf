# Building serf

serf has two build shapes, and the difference matters.

## 1. Default build — fast, runs today

```sh
cargo build --release          # ~10 min on a Pinebook Pro, CARGO_BUILD_JOBS=2
cargo test                     # unit tests: config, adblock, vim, hints, ipc, overlay, session
```

This links **everything except the web engine**: the Wayland window, the
`wlr-layer-shell` overlay, the modal input machine, link hinting, the
`serf-msg` control channel, hot-reloaded config, the theming/pywal pipeline
and the full adblock engine (real `adblock-rust`, real filter lists, real
memory-mapped cache).

The window comes up and every control path works, but the page area is
driven by `StubEngine` — it records navigation instead of rendering a web
page. `serf-msg status`, tab handling, scrolling, hints and mode switching
are all exercisable against it.

## 2. Servo build — the real engine, and why it is expensive

```sh
cargo build --release --features servo
```

Turning on `--features servo` pulls in the `servo` crate (currently 0.7).
That is **776 transitive crates**, and the critical one is `mozjs_sys`:
Servo embeds SpiderMonkey, and `mozjs_sys` builds the SpiderMonkey C++
engine **from source** on aarch64 (it looks for a prebuilt archive via
`MOZJS_ARCHIVE`; none is published for this target).

Consequences on the Pinebook Pro (RK3399, 4×A53 + 2×A72, ~3.7 GB RAM):

* SpiderMonkey alone is a multi-hour compile and wants far more than the
  ~1 GB of RAM this box has free (it already swaps hard at idle).
* The link step for `libmozjs` is the peak-RAM moment; it is the most
  likely place to OOM.

**Do not start this build on the Pinebook Pro.** Build it on an aarch64
machine with ≥16 GB RAM and plenty of cores — a Scaleway ARM instance
(COPARM1 “Ampere Altra”) or a native `ubuntu-24.04-arm` GitHub runner do
fine. Cross-compiling from x86-64 is *possible* but the `mozjs_sys` build
script is hostile to cross builds; native aarch64 is far less painful and
produces a binary that actually runs on the PBP.

`scripts/build-servo.sh` wraps that build (fetch, build, report timings).

## Cross-checking the Servo backend code

The `servo_backend` module in `src/browser/engine.rs` is written against the
`servo 0.7` embedding API as used by Servo’s own
`examples/winit_minimal.rs` (window rendering context → `ServoBuilder` →
`WebViewBuilder`). It is **not compiled by the default build** and has not
been compiled in the session that authored it — run the Servo build before
trusting it.
