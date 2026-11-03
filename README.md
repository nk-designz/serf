# serf — a suckless servo browser for Wayland

A hyper-minimalist, keyboard-driven web browser built on Servo, for tiling
Wayland compositors (Hyprland, Sway, River) and low-resource ARM64 hardware
(the Pinebook Pro's RK3399 in particular).

The design follows surf: **there is no browser chrome**. The viewport is 100%
page. Address input, tab switching and status live in a `wlr-layer-shell`
bar that appears on demand, driven by keys or by `serf-msg` over a UNIX
socket.

## What is in the tree

```
serf/
├── Cargo.toml
├── .cargo/config.toml        # cortex-a72 + NEON target flags, JOBS=2
├── config.example.toml       # annotated, hot-reloadable config
├── BUILD.md                  # the two build shapes and their costs
├── scripts/build-servo.sh    # the heavy Servo build, for a big ARM box
└── src/
    ├── main.rs               # winit (Wayland) event loop + surface init
    ├── config.rs             # TOML config, shared state, live reload
    ├── ipc.rs                # serf-msg UNIX-socket server + client
    ├── bin/serf-msg.rs       # the serf-msg command-line client
    ├── browser/
    │   ├── mod.rs
    │   ├── engine.rs         # Engine trait, StubEngine, Servo backend
    │   └── navigation.rs     # tabs, per-tab history, IPC command dispatch
    ├── input/
    │   ├── mod.rs
    │   ├── vim.rs            # NORMAL / INSERT / COMMAND state machine
    │   └── hints.rs          # prefix-free link-hint generation + session
    ├── adblock/
    │   └── mod.rs            # adblock-rust + mmap'd lists + cosmetic CSS
    └── ui/
        ├── mod.rs
        ├── overlay.rs        # wlr-layer-shell status bar + 5x7 bitmap font
        └── theme.rs          # pywal / catppuccin palette, CSS injection
```

## How the requirements map to the code

* **Pure Wayland, no GTK/Qt/X11.** `winit` is built with
  `default-features = false, features = ["wayland", "rwh_06"]`, so no X11
  backend is even linked. The status overlay speaks `wlr-layer-shell`
  directly through `wayland-client` + `wayland-protocols-wlr`
  (`ui/overlay.rs`) on its own connection and thread.

* **Zero-UI core.** The window is the page; the overlay is a separate
  layer-shell surface (`Layer::Top`, `KeyboardInteractivity::None`) that is
  only shown when there is something to say (`OverlayState::set_visible`).

* **Vim modality.** `input/vim.rs` is a real state machine with three modes,
  numeric count prefixes (`3j`), and multi-key chords (`g g`). Bindings come
  from the config, layered over built-in defaults.

* **Engine-level adblocking.** `adblock/mod.rs` wraps the same `adblock-rust`
  engine Brave ships. `check()` is the network hook (block / redirect /
  rewrite / CSP); `cosmetic_css()` is the layout hook. Filter lists are
  compiled once and **memory-mapped** (`from_cache`) instead of re-parsed,
  which is what keeps the adblock footprint small.

* **Hot-reloadable config.** `config::spawn_watcher` watches the config with
  `notify` and swaps a new `Config` into an `Arc<RwLock<_>>` — no restart.
  The palette is re-resolved on `theme reload`, which is the pywal hook:
  pywal rewrites `~/.cache/wal/colors.json`, `serf-msg theme reload` picks
  it up.

* **Low-resource performance.** `mimalloc` is the global allocator; the
  release profile uses thin LTO with `codegen-units = 1`; the build targets
  `cortex-a72` with NEON.

* **Servo embedding.** `browser/engine.rs` defines the `Engine` trait and a
  real `servo_backend::ServoEngine` written against the `servo 0.7` API —
  gated behind `--features servo`, because that build compiles SpiderMonkey
  from source (see BUILD.md).

## Status

The default build compiles and runs; its unit tests exercise config parsing
and hot-reload plumbing, the adblock engine against real filter syntax
(including the mmap cache round trip), the vim state machine, hint
generation/selection, the IPC socket, the status renderer and the tab model.

The Servo backend is written but *not yet compiled* — it requires the
SpiderMonkey build. See BUILD.md.
