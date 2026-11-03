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

## 1.6 Platform APIs: implemented in Servo, switched off in serf — **VERIFIED**

The most misleading thing about this project's status. Servo 0.7 implements far
more than serf turns on. Its preferences are macro-generated `bool` fields with
defaults (`servo-config-0.7.0/prefs.rs`), and **serf passes no `Preferences` at
all** (`ServoBuilder::default()` in `src/main.rs`), so every default applies —
and the defaults are largely *off*.

Interfaces are gated by name in the WebIDL (`Pref="dom_x_enabled"` in
`servo-script-bindings-0.7.0/webidls/`), so the gate is authoritative and
mechanically checkable.

### How to turn any of this on

`Preferences` has `pub` fields and a `Default` impl
(`servo-config-0.7.0/prefs.rs:615`), and `ServoBuilder::preferences()` accepts
one (`servo-0.7.0/servo.rs:1476`). So it is a type-safe struct literal, not
string key fiddling:

```rust
ServoBuilder::default()
    .preferences(servo::preferences::Preferences {
        dom_indexeddb_enabled: true,
        dom_serviceworker_enabled: true,
        ..Default::default()
    })
```

`Servo::set_preference(name, PrefValue)` also exists for runtime changes, but the
name format it expects would need checking; the struct literal avoids the
question and covers startup.

**Caveat, and it is a real one:** enabled ≠ working. Some paths behind these
prefs are `unimplemented!()` / `todo!()` and will **panic** if reached —
IndexedDB has one on `multiEntry` keys
(`dom/indexeddb/key.rs:813`). So switch prefs on deliberately and actually
exercise the feature; do not flip the whole set at once and assume it is fine.

### The state of each area

| area | state | note |
|---|---|---|
| **cookies / sessions** | `document.cookie` works | **not** pref-gated (plain `attribute DOMString cookie;` in `Document.webidl`). The new `cookieStore` API *is* gated (`dom_cookiestore_enabled=false`) — that is the modern API only, not cookies themselves. |
| cookie persistence | **broken** | no `Opts.config_dir`, so cookies are in-memory. See §1.4. This is the §1.4 fix; nothing else needed. |
| **HTTP cache** | on, but RAM only | `network_http_cache_disabled=false`. Servo's own `NetworkManager` docs say an on-disk cache is "under development", so every restart is cold. Not fixable from serf. |
| Cache API / CacheStorage | **off** | gated by `dom_serviceworker_enabled`; moot without service workers. |
| **WebSQL** | absent | not in Servo's DOM at all, and not a gap — Web SQL Database is a withdrawn spec no browser should still ship. |
| **IndexedDB** | **implemented, off** | `dom/indexeddb/` is 13 files / ~6.5k lines, not a stub, but `dom_indexeddb_enabled=false` gates 12 interfaces. Sites using it for local state get nothing today. |
| **service workers** | **implemented, off** | `dom/serviceworker/` is 14 files / ~4.8k lines with no `todo!`/`unimplemented!` stubs, but `dom_serviceworker_enabled=false` gates 13 interfaces including `navigator.serviceWorker` and `caches`. Wants persistent storage (§1.4) to be meaningful. |
| **notifications** | **implemented, off, and serf lacks the hook** | fully plumbed: `Notification.webidl` is pref-gated by `dom_notification_enabled=false`, `PermissionFeature::Notifications` exists, and the display path is real — `EmbedderMsg::ShowNotification(Option<WebViewId>, Notification)` and **`ServoDelegate::show_notification(&self, Notification)`** (`servo-0.7.0/servo_delegate.rs:40`). serf implements neither that nor permission prompting. |
| permissions | **off** | `dom_permissions_enabled=false`; also needed for notifications to be grantable. |

To actually *display* a notification on this desktop, the embedder has to talk to
`org.freedesktop.Notifications` over D-Bus (mako/dunst on wlroots/scenefx) —
Servo hands serf the `Notification` and stops. ~50 lines, or shell out to
`notify-send`. `PermissionFeature` lists the full set an embedder must decide on:
`Geolocation, Notifications, Push, Midi, Camera, Microphone, Speaker, DeviceInfo,
BackgroundSync, Bluetooth, PersistentStorage, ScreenWakeLock, Gamepad`.

### And the ones that quietly hurt ordinary browsing

These are *not* exotic. They are off by default today, and several are used by
ordinary sites:

| API | default | consequence |
|---|---|---|
| `dom_webgl2_enabled` | **false** | only **WebGL1**. Lots of 3D/canvas work needs WebGL2. |
| `dom_intersection_observer_enabled` | **false** | breaks lazy-loading images and infinite scroll — sites look broken/incomplete. |
| `dom_web_animations_enabled` | **false** | no `element.animate()`; a very common animation/interaction path. |
| `dom_composition_event_enabled` | **false** | **no IME** — CJK input and dead keys in text fields do not work. Only matters if you type a non-Latin script, then it is fatal. |
| `dom_exec_command_enabled` | **false** | `document.execCommand` gone → in-page "copy" buttons and some editors degrade. |
| `dom_async_clipboard_enabled` | **false** | no `navigator.clipboard` (the *browser-level* clipboard still works — see §3). |
| `dom_fontface_enabled` | **false** | JS-loaded web fonts (`new FontFace(...)`) fail. |
| `dom_worklet_enabled` | **false** | no AudioWorklet / PaintWorklet / Houdini. |
| `dom_webrtc_enabled`, `dom_geolocation_enabled`, `dom_bluetooth_enabled`, `dom_wakelock_enabled`, `dom_credential_management_enabled`, `dom_visual_viewport_enabled`, `dom_offscreen_canvas_enabled`, `dom_storage_manager_api_enabled` | false | mostly acceptable for browsing; wake lock matters for video. |

On by default and working: `localStorage` / `sessionStorage` (not gated at all),
`dom_crypto_subtle_enabled` (**SubtleCrypto works** — good, many sites need it),
`dom_resize_observer_enabled`, `dom_sharedworker_enabled`, `dom_canvas_text_enabled`.

`dom_gamepad_enabled` and `dom_webxr_enabled` default to *true* but are moot:
servo's `gamepad` / `webxr` / `webgpu` **cargo features** are compiled out in
serf (only `brotli-compression-stream`, `webgl`, `webcrypto`, `media-gstreamer`
are added on top of servo's defaults — and the defaults do include `clipboard`,
which is why the clipboard works).

**Action:** turning this around is mostly a `Preferences` table in
`create_engine` plus per-API verification, not a coding project. Treat it as its
own pass with its own CI round, and enable in small batches so a panic tells you
which API caused it.

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
2. **`Opts.config_dir` + a `Preferences` table** (§1.4, §1.6) — one change, and it
   buys persistent logins *and* WebGL2, IntersectionObserver, Web Animations,
   IndexedDB, service workers and notifications. Highest value per line in this
   file — but enable prefs in small batches and actually exercise each one, since
   some paths behind them panic (§1.6).
3. **Real modal dialogs** (§1.1) — reuses the launcher and overlay, removes a
   whole class of silent misbehaviour.
4. **mpv hand-off keybinding** (§4) — small, and gives video immediately.
5. **Notification display** (§1.6) — needs `show_notification` plus a D-Bus call
   to `org.freedesktop.Notifications` and permission prompting; only worth doing
   once the prefs are on.
6. **Network-level adblocking** (§2.1) — the mission spec's outstanding item.
7. Then §1.2/§1.3 (downloads, popups) — the two hardest, and both may need
   research before they need code.
