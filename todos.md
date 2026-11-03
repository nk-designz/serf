# serf — what stands between this and a daily driver

A working list. Every item carries the *reason* it is on the list, and the reason
is either **VERIFIED** (read out of the crate source in the local registry, or run
on this machine, with a file/line or a command) or marked **ASSUMED** (a plan
that has not been checked yet). Nothing here is a guess dressed as a fact — the
whole point of writing it down is that the verified parts were expensive to get
and should not be re-derived.

See also `docs/daily-driver.md` for the original research notes.

---

## 0. Where things actually stand

**The Servo engine code has never executed.** That is the single most important
line in this file. Everything below is a plan built on top of an unverified base.

| | state |
|---|---|
| default build (StubEngine) | green, runs, 98 tests pass — but renders nothing |
| servo build compiles | **not yet proven.** Round 1 reached serf's code for the first time and found 2 API errors (fixed in `a54539a`); round 2 is in flight |
| servo build has ever *run* | no |
| newest servo artifact | run `41622127882`, sha `462f1ea`, `serf-aarch64-servo` 48.65 MB — **predates the delegate**, so it paints no frames |

The three things that have never been exercised, in the order they are likely to
break:

1. **GL over Panfrost.** WebGL is enabled (mozangle/ANGLE). Mali-T860 is Midgard
   v5, no Vulkan. No one has confirmed a frame reaches the screen on this GPU.
2. **The `wlr-layer-shell` overlay.** Written against raw `wayland-client`, never
   mapped on a real compositor (mango/wlroots/scenefx).
3. **The input plumbing.** `notify_input_event` / `notify_scroll_event` /
   `set_focused` are all new and unexercised.

Expect the first live run to be a debugging session, not a demo.

### Verifying a change without a browser

```sh
CARGO_BUILD_JOBS=2 cargo check --all-targets   # the fast loop, seconds
CARGO_BUILD_JOBS=2 cargo test                  # 98 tests, seconds
CARGO_BUILD_JOBS=2 cargo build --bins          # target/debug/{serf,serf-msg}
```

The `servo` feature is the one thing this box cannot compile (4 GB, ~950 crates
with SpiderMonkey), so **CI is the compiler for engine code, not a final gate.**
Push early and iterate on the runner's errors.

**Attribute a CI failure before fixing anything.** ~950 crates compile before the
build reaches serf. Read *which crate* failed:

```sh
gh run view <id> --log-failed | grep -oE 'could not compile `[a-z0-9_-]+`'
```

`could not compile \`serf\`` means the dependency chain is good and everything
below is a real serf/API mistake. Any other crate name means the build never got
to serf and no serf error has been reported yet. Getting this wrong sends you
chasing a dependency pin when your own code is fine (or the reverse).

Logs are ANSI-escaped and prefixed `job<TAB>step<TAB>TIMESTAMP `; GNU `sed` and
Python `\x1b` both failed to strip them here, `perl -pe 's/\e\[[0-9;]*m//g'`
works. Strip ANSI before grepping for `^error` or you get an empty result and
conclude there were no errors.

The `servo` job now sets `CACHE_ON_FAILURE: "true"` — without it a failed build
discards the dependency cache and every retry pays the full ~7 minute recompile.
Warm, the whole job is ~15 min end to end.

---

## 1. P0 — these are what stop it being usable

### 1.1 Modal dialogs are auto-answered, not shown — **VERIFIED**

`alert()` / `confirm()` / `prompt()` are answered immediately: alert gets OK,
confirm and prompt get "cancel", all three logged. The reason is not laziness —
an unanswered dialog freezes the page on Servo's side, so *not* answering is
worse than answering wrongly.

**Why it matters:** `confirm()` gates destructive actions and `prompt()` is a
real input on some sites, so today those flows silently take the cancel branch.

**Approach:** the types are all public and the plumbing already exists —
`SimpleDialog`, `ConfirmDialog`, `PromptDialog`, `AlertDialog` are re-exported
from `servo` (`servo-0.7.0/lib.rs`, the `webview_delegate` `pub use` block).
serf already has the pieces to render them with zero new UI:

- `prompt()` → the external launcher (it accepts a typed line that is not in the
  list, which is exactly a text prompt);
- `confirm()` → launcher with two entries;
- `alert()` → the overlay status line, dismissed with Escape.

That reuses `src/browser/launcher.rs` and the overlay rather than adding widgets,
which is the whole point of the architecture. **Cost: small.** Good first task.

### 1.2 Downloads cannot be implemented — **VERIFIED, needs research**

There is **no download API in Servo 0.7's embedder surface at all**. Not a hook
serf skipped: `grep -rn Download servo-embedder-traits-0.7.0/` and the same over
`servo-0.7.0/*.rs` return nothing. So clicking a download link does nothing, and
no amount of serf-side work changes that via a "download" callback.

**The candidate route (ASSUMED, investigate):** the resource-interception API
may be enough to build downloads ourselves. `WebViewDelegate::load_web_resource`
hands us the `WebResourceRequest` (method, headers, url, destination,
`is_for_main_frame`) *before* the response, and `WebResourceLoad::intercept`
lets us supply the response. A download is detectable — `load_web_resource` sees
the request, and the response path can tell us the `Content-Disposition`. If we
intercept, fetch it ourselves, and write the body to `~/Downloads`, that is a
real download manager in ~100 lines without any Servo support. **Unverified and
non-trivial**: we would be doing the HTTP ourselves, including redirects, TLS
and progress. Find out whether a request can be *observed* without being
*replaced* before committing to this.

### 1.3 Popups are adopted as tabs, and `window.opener` is broken — **VERIFIED**

`request_create_new` gives a `CreateNewWebViewRequest` whose `.builder(ctx)` is
used to construct the child, so the page's handle is real and `win.closed`
works. But we never paint it — serf renders exactly one view — and we adopt the
URL as a **new tab** instead. A new tab is a *different* web view, so
`window.opener` messaging between the two does not work.

**Why it matters most:** popup-based sign-in and OAuth flows are common, and
they break in a way that looks like the site being broken.

**Approach (ASSUMED):** three options, increasing cost —
(a) forward `postMessage`/opener calls between the two views if Servo routes
them automatically, which needs checking first; (b) actually paint the child in
a temporary view; (c) keep it adopted and accept the limitation, documented.
Check (a) before building anything: it may be free.

### 1.4 Nothing persists across a restart — **VERIFIED, cheap fix**

serf builds `ServoBuilder::default()` with **no `Opts`** (`src/main.rs`,
`create_engine`). `Opts::default()` sets `config_dir: None`
(`servo-config-0.7.0/opts.rs:256`), so there is no profile directory. Cookies
and localStorage are in-memory only: you can log into a site and it works for
that session, then it is gone.

**Fix:** pass `Opts` with `config_dir = ~/.local/share/serf/profile` to
`ServoBuilder::opts(...)`. `Opts` is re-exported from `servo`
(`servo::Opts`), `config_dir` is a `Option<PathBuf>`
(`servo-config-0.7.0/opts.rs:66`), and it is threaded through at
`servo-0.7.0/servo.rs:993`. **Cost: small + one CI round.** Do it with the next
engine push.

**Also missing, same theme:** no session restore. Tabs are in-memory and closing
the window loses everything. Needs serf-side state (a `session.toml` next to the
config), independent of Servo.

### 1.5 HTTP authentication is dropped, by design — **VERIFIED**

A 401 raises `AuthenticationRequest`; serf drops it, which cancels the request,
so any site behind basic auth fails. This is deliberate — serf collects no
credentials — and `AuthenticationRequest::authenticate(username, password)` is
available if that changes.

**Decision needed, not code needed.** A launcher-based prompt is possible
(passwords would travel via the child's stdin, not argv, so they would not show
in `ps`), but a dmenu-alike is a poor place to type a password and the value
should not be echoed. Simplest honest position: leave it off, document it. Sites
that need it are mostly internal/dev services.

---

## 2. P1 — the difference between "works" and "good"

### 2.1 Network-level adblocking — **VERIFIED API, not implemented**

Today blocking is **cosmetic only**. `AdblockEngine::check()` exists and is unit
tested but **is never called**; `load_web_resource` is not overridden, so ads and
trackers still load and execute even where CSS hides them. Cabinets the mission
spec's "adblock-rust into Servo's network request hooks".

Everything needed is known and does not have to be re-discovered:

- **The hook:** `WebViewDelegate::load_web_resource(&self, webview: WebView,
  load: WebResourceLoad)` and, for loads not tied to a view (service workers),
  `ServoDelegate::load_web_resource(&self, load: WebResourceLoad)`. Both have
  default no-op bodies; look at `servo-0.7.0/webview_delegate.rs` (~line 1067)
  and `servo-0.7.0/servo_delegate.rs` (~line 37).
- **Blocking a load:** `load.intercept(response).cancel()` — `cancel()` triggers
  a network error. `intercept` takes a `WebResourceResponse`; there may be a way
  to return an empty body instead of an error, which would be cleaner for the
  page's console. Check `WebResourceResponse`'s impl before choosing.
- **The request:** `WebResourceRequest { method, headers, url, destination,
  referrer_url, is_for_main_frame, is_redirect }` in
  `servo-embedder-traits-0.7.0/lib.rs:676`. `destination` is a
  `content_security_policy::Destination` (~25 variants: Document, Image, Script,
  Style, Font, …). **Servo does not re-export this type**, so mapping it needs a
  direct, optional dependency on `content-security-policy = "0.9"` — already in
  the lock at 0.9.0 via servo, so it costs no new crate.
- **Thread affinity, which decides the whole design:** the delegate callbacks
  run on the **embedder thread**, not on Servo threads. Proof: the delegate is
  stored as `Rc<dyn WebViewDelegate>` inside a `RefCell` (`servo-0.7.0/webview.rs:101`,
  `servo-0.7.0/servo.rs:244`) — `Rc`/`RefCell` are not `Send`/`Sync`, so this is
  only sound single-threaded — and the dispatch site is
  `handle_net_embedder_message` (`servo-0.7.0/servo.rs:415-456`), called from
  `spin_event_loop` on the loop that owns the delegate.

  **Consequence:** `adblock::Engine` is not `Send` (it holds an `Rc`) and that is
  *fine* — it can be consulted synchronously inside `load_web_resource`. Do not
  build a channel or a second engine for thread-safety reasons; there is no
  cross-thread problem. Keep **one** engine and share it as `Rc<AdblockEngine>`
  between `App` (cosmetic CSS) and the delegate (network blocking), so the
  list-loading cost and memory are paid once. That also keeps the <15 MB adblock
  budget in the mission spec reachable.

**Cost: medium.** ~150 lines plus one CI round. The main risk is guessing the
`WebResourceResponse` shape for a clean "blocked" response.

### 2.2 History and bookmarks

Per-tab history exists in the model (`src/browser/navigation.rs`) but is
in-memory. Needs an on-disk store, plus a UI to see it. `H`/`L` navigate within
a session only.

### 2.3 Find-in-page, zoom, and the other small ones

`/` is bound to *search the web*, not find-in-page. Zoom exists as a config value
but no keybinding changes it. Neither is hard; both are noticed immediately in
daily use.

### 2.4 User agent

Nothing sets one, so sites see Servo's default UA and some will serve degraded
pages or refuse. Worth a config key, and worth checking what the default
actually is before deciding it is a problem.

### 2.5 Cache and proxy

The HTTP cache is in-memory only (`servo-0.7.0/network_manager.rs` notes an
on-disk cache is "under development"), so every restart starts cold. No proxy
support. Both are Servo-side or `Opts`-side; document rather than fight.

---

## 3. Verified facts worth not re-deriving

Written down because each cost a CI round.

- **servo does not re-export `Url`.** `use servo::Url` is `E0432 no Url in the
  root`. `WebResourceRequest.url` and the navigation callbacks use the **`url`
  crate's** `Url` (`servo-embedder-traits-0.7.0/lib.rs:44`). serf already
  depends on `url`.
- **servo does not re-export `KeyboardEvent`, and it is a different type.**
  servo re-exports only the primitives —
  `pub use keyboard_types::{Code, CompositionEvent, CompositionState, Key,
  KeyState, Location, Modifiers, NamedKey}` (`servo-0.7.0/lib.rs`) — and defines
  its own
  `pub struct KeyboardEvent { pub event: ::keyboard_types::KeyboardEvent }`
  (`servo-embedder-traits-0.7.0/input_events.rs:148`). Adding `keyboard-types` at
  servo's own version does **not** make them interchangeable; the raw event must
  be wrapped with `servo::KeyboardEvent::new(raw)`.
- **`Modifiers::SUPER` is deprecated** — use `META`.
- **Clipboard works out of the box.** servo ships `DefaultClipboardDelegate` and
  the docs state that on Linux with the `clipboard` feature enabled a default
  delegate implementing system clipboard support is used automatically
  (`servo-0.7.0/clipboard_delegate.rs`). `clipboard` is in our feature set, so
  copy/paste is the real thing, not an in-process buffer. No work needed.
- **`ServoBuilder` can only set:** `opts`, `preferences`, `event_loop_waker`,
  `protocol_registry` (`servo-0.7.0/servo.rs`).
- **`NetworkManager` is cache control only** (`cache_entries`, `clear_cache`) —
  it is *not* a request interceptor. Do not go looking there for adblocking.
- **The pkcs1 pin must be `=0.8.0-rc.4`, not rc.5 and not 0.8.0.** `servo-script`
  pins `rsa = "=0.10.0-rc.18"`, whose pkcs1 interop needs
  `pkcs1::RsaPrivateKey<'a>` (a lifetime); rc.5 and 0.8.0 changed it to a generic
  `<U>` and fail with 8 × E0107. rc.4 is the one with the lifetime. Verify by
  compiling rsa both ways in a throwaway crate — and note that a "local proof"
  only counts if the crate enables rsa's `encoding` feature, which is what gates
  that code (a `default-features = false` test crate silently proves nothing).

---

## 4. YouTube and video playback

**Short answer: no, and the blocker is structural.** Do not spend time on codec
tuning for YouTube.

### Why YouTube cannot work

**Media Source Extensions are absent from Servo 0.7 — VERIFIED.** YouTube's web
player feeds video through `SourceBuffer` over MSE for essentially all playback.
In the entire `servo-script` crate, `MediaSource` appears exactly once, as a TODO:

```
servo-script-0.7.0/dom/html/embedded_content/htmlmediaelement.rs:2095
  // TODO Step 7.3. If the media element's assigned media provider object is a MediaSource
```

There is no `MediaSource`/`SourceBuffer` implementation file anywhere in the DOM
layer, and no interface is exposed to JS. YouTube's player feature-detects that
and has no usable fallback — it stopped serving progressive MP4 to the web long
ago. **This is a Servo limitation, not a serf bug, and serf cannot patch it.**

### Codec and hardware reality — VERIFIED on this box

Checked by looking at the plugin `.so` files, **not** with `gst-inspect-1.0`,
which is not installed on this box (asking it made every codec look missing —
the verdict was about the missing tool, not the codecs).

| plugin | state | meaning |
|---|---|---|
| `libgstvpx.so` | present | VP8/VP9 software decode |
| `libgstopenh264.so` | present | H.264 software decode (openh264) |
| `libgstav1.so` | **absent** | no AV1 at all |
| `libgstlibav.so` | **absent** | no libav fallback |
| `libgstv4l2codecs.so` | present | the V4L2 **hardware**-decode path |
| `libgstisomp4.so`, `libgstmatroska.so` | present | MP4 and WebM/Matroska demux |
| `libgstopus.so`, `libgstfaad.so` | present | Opus and AAC audio |
| `libgstpulseaudio.so` | present | audio output works (PipeWire + Pulse running) |
| `libgstalsa.so` | absent | no ALSA sink — irrelevant, pulse sink exists |

So containers, demux and audio are fine. The problems are decode **speed** and
format: VP9 here is **software only**, and 2×A72 decoding 1080p VP9 is not
watchable. YouTube prefers VP9 or AV1, and AV1 we cannot decode at all. The
RK3399 has a real video decoder and `libgstv4l2codecs.so` is present, so
hardware decode is a plausible future project — **but it would not unblock
YouTube either, because MSE is the wall, not the codec.**

`media-gstreamer` is in the serf `servo` feature set, so Servo *can* play a plain
progressive `<video src="...">`. That is the class of video that plausibly works
today: a non-DRM, non-MSE MP4 or WebM with H.264/VP9 + AAC/Opus. Untested, since
the binary has never run.

### The pragmatic route: hand it to mpv

This is the same pattern serf already uses for the tab launcher — do not embed,
hand off. **`mpv` is installed on this box; `yt-dlp` is NOT**, and mpv needs
yt-dlp to resolve YouTube URLs.

1. `sudo apt install yt-dlp` (one package).
2. A keybinding that hands the current URL to
   `mpv --ytdl-format='bestvideo[height<=?720]+bestaudio/best' <url>`, spawned
   exactly like `tab_launcher` is today (`sh -c`, own thread, never block the
   winit loop — reuse `src/browser/launcher.rs`'s structure).
3. The height cap matters on this hardware: software VP9 will not do 1080p, and
   mpv's own yt-dlp integration picks the format, so it needs constraining.

This gives working YouTube (and every other yt-dlp-supported site) without
touching Servo's media stack, and it is consistent with the suckless design.
**Cost: small.** The best value in this whole file per line of code.

### If native playback is ever wanted

- VP9 software decode exists but will not do 1080p on 2×A72. Expect ≤480p.
- Wiring the RK3399 VPU through `v4l2codecs` is its own project, with driver
  work, and still does not fix MSE.
- Native YouTube ultimately needs either Servo to implement MSE, or a
  serf-side MSE shim feeding decoded frames into the DOM — both are large.

---

## 5. Priority order

1. **Confirm the engine binary runs at all** — GL/Panfrost, the overlay, input.
   Nothing above matters until this is true.
2. **`Opts.config_dir`** (§1.4) — one line, makes logins survive a restart.
3. **Real modal dialogs** (§1.1) — reuses the launcher and overlay, removes a
   whole class of silent misbehaviour.
4. **mpv hand-off keybinding** (§4) — small, and gives video immediately.
5. **Network-level adblocking** (§2.1) — the mission spec's outstanding item.
6. Then §1.2/§1.3 (downloads, popups) — the two hardest, and both may need
   research before they need code.
