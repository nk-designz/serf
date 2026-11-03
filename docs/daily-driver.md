# Making serf a daily driver on the Pinebook Pro

Research notes, checked against `servo-0.7.0` in the local registry and against
the Mesa/Panfrost docs — not from memory. Headline: **the blocker is mostly not
Servo and mostly not hardware. It is that serf does not yet forward input or
handle any embedder callbacks.** Two of the three workloads named are blocked by
the *GPU driver*, not by us.

---

## 0. The thing that matters most: serf is currently a screenshot viewer

Servo's own embedding contract (`doc.servo.org/servo/`) says, of `WebViewBuilder`:

> The builder **must** be provided with a `WebViewDelegate` implementation which,
> at a minimum, should handle `WebViewDelegate::notify_new_frame_ready` by calling
> `WebView::paint` and presenting the rendered page using `RenderingContext::present`.

and, of the event loop:

> Your application's event handlers need to forward input events for a particular
> `WebView` using one of the `notify_*` methods.

serf does **neither**. `create_webview` builds a `WebView` with no `.delegate(...)`,
and nothing anywhere calls `notify_input_event`. Consequences:

- nothing drives repaints — we paint once and only if something happens to call
  `present()`;
- clicks, keys, mouse move and wheel go nowhere;
- our current scrolling is a JS injection (`window.scrollBy`) instead of
  `WebView::notify_scroll_event`.

**This is P0. Everything below is irrelevant until it is done**, and it is our
code, not an engine limitation.

---

## 1. P0 — the embedder surface to implement

### 1.1 A `WebViewDelegate`

`servo::WebViewDelegate` has ~35 default-no-op methods. The ones a daily driver
needs, roughly in order of how much you feel their absence:

| callback | what breaks without it |
|---|---|
| `notify_new_frame_ready` | **no repaints at all** (call `paint()` + `present()`) |
| `notify_url_changed` | the status line's URL never updates |
| `notify_page_title_changed` | tab titles stay blank |
| `notify_status_text_changed` | surf's whole point: hover URL not shown |
| `notify_load_status_changed` | no progress/throbber, no "loading" state |
| `notify_cursor_changed` | always the default cursor; links don't look clickable |
| `notify_input_event_handled` | **vim keys can't chain properly** — this is how Servo hands back keys it did *not* consume, which is exactly the hook for a modal keymap |
| `request_create_new` | `target="_blank"` / `window.open` do nothing (Booking.com uses these) |
| `show_embedder_control` | `<select>` dropdowns, colour pickers, **file upload.** Booking.com is form-dense; this is not optional there |
| `request_navigation` | `beforeunload` ("leave site?") silently ignored |
| `request_permission` | geolocation/camera/notifications can never be granted (Booking.com asks for location) |
| `notify_history_changed`, `notify_traversal_complete` | our tab history and Servo's session history drift apart |
| `notify_fullscreen_state_changed` + `exit_fullscreen` | no video fullscreen |
| `notify_favicon_changed` | no favicons in the status line |
| `notify_crashed` | a content crash is a silent hang |
| `show_console_message` | no way to debug a site from inside serf |
| `request_authentication` | HTTP auth / captive portals fail |
| `show_notification` | web notifications silently dropped |
| `notify_media_session_event` | media keys / OS media controls |

### 1.2 Input forwarding

`embedder_traits::InputEvent` variants we can forward:
`Keyboard`, `MouseButton`, `MouseMove`, `MouseLeftViewport`, `Wheel`, `Touch`,
`Ime`, `EditingAction` (clipboard), `Gamepad`.

`WebView::notify_input_event(event) -> InputEventId`, keep the id, and Servo
calls `notify_input_event_handled(id, result)` when it is done — so unhandled
keys can fall through to the vim keymap instead of being guessed at.

### 1.3 Scrolling properly

Replace the JS `window.scrollBy` hack with
`WebView::notify_scroll_event(scroll, point)`. The JS version only moves the
document scroll position, so it cannot scroll a scrollable *sub-element* (the
YouTube comments pane, any `overflow:` container) and it fights the compositor's
own scroll model. Also relevant: `adjust_pinch_zoom` and `set_page_zoom`
(the latter already used).

### 1.4 Also missing for real use

- `ClipboardDelegate` (separate trait; the `clipboard` cargo feature is on by
  default) — copy/paste.
- Session persistence: cookies, `localStorage`, `IndexedDB` across restarts
  (`SiteDataManager`). Without it every site forgets you on every launch.
- Downloads and uploads.

---

## 2. YouTube: blocked by the *media build*, not by our code

Checked `servo-0.7.0/Cargo.toml`:

```
default_web_features = ["brotli-compression-stream", "clipboard",
                        "webcrypto", "webgpu", "webgl", "webxr"]
media-gstreamer = ["servo-media-gstreamer", "gstreamer"]
```

`media-gstreamer` is **not** a default feature — and the CI log shows
`servo-media-dummy` being compiled, i.e. the build we have ships a **dummy media
backend**. `<video>` currently plays nothing.

To even attempt YouTube you need `--features media-gstreamer` plus the GStreamer
dev stack, and then:

- YouTube uses **MSE** (Media Source Extensions) to feed the `<video>` a few
  seconds of data at a time. Servo's MSE support is partial, and this is engine
  work, not embedder work — we cannot fix it from serf.
- Codecs: VP9/AV1 or H.264 plus Opus/AAC. On RK3399 there *is* hardware video
  decode (rkvdec/Hantro via V4L2), but it needs the right GStreamer plugins;
  software VP9 at 1080p on 2×A72 is not realistic. 480p, maybe.
- **No DRM.** No Widevine, so DRM-protected content (Netflix, Spotify, some
  YouTube) can never work.

Verdict: **not a daily-driver feature.** Plain `<video>`/`<audio>` pages are
plausible; YouTube specifically is unreliable and depends on upstream MSE
progress.

---

## 3. WebGPU: blocked by this machine's GPU driver

Servo's WebGPU is behind `dom.webgpu.enabled`, is "work in progress"
(`servo/servo` project 24), and is built on **wgpu** — which on Linux ARM means
Vulkan or GL.

The Pinebook Pro's GPU is a **Mali-T860, Midgard v5**. From the Mesa Panfrost docs'
supported-hardware table, the Vulkan column is **empty for T860**:

| Model | Arch | OpenGL ES | OpenGL | Vulkan |
|---|---|---|---|---|
| T760, T820, T830, **T860**, T880 | Midgard (v5) | 3.1 | 3.1 | *(none)* |
| G72 | Bifrost (v6) | 3.1 | 3.1 | 1.3 (experimental) |

PanVK only covers Bifrost v6/v7 and Valhall, and even there it is experimental —
it refuses to load unless you set `PAN_I_WANT_A_BROKEN_VULKAN_DRIVER=1`. Midgard
has **no Vulkan driver at all**. So there is no Vulkan device for wgpu to bind to.

This is the same wall a field report hit on a different ARM board (Toradex Luna,
PowerVR):

> The PowerVR Vulkan driver lacks display WSI, so it cannot display on
> wgpu/Vulkan. Resolved by discarding Vulkan and aligning both Slint and Servo to
> OpenGL ES.

Verdict: **WebGPU workloads will not run on the Pinebook Pro.** Not a serf bug,
not a Servo bug — no Vulkan driver exists for Midgard. The only theoretical path
is wgpu's experimental GLES backend, which Servo does not target. Treat as
out of scope; do not spend time here.

---

## 4. Rendering: GLES is the right choice, and we already made it

Mali-T860 gives OpenGL ES 3.1 (officially non-conformant, but it works), and we
use `WindowRenderingContext` with surfman's GLES path — correct. The Toradex
measurement backs this up: moving from software rendering to GLES cut CPU from
300–380% across 4 cores to ~100% (≈1 core). `SoftwareRenderingContext` exists in
the API and is worth keeping as a diagnostic fallback, not as a default.

Do **not** enable `webgpu`/Vulkan prefs on this box (see §3).

---

## 5. Memory: the real ceiling on a 4 GB box

Measurements from the Servo tracker (issue #5892, 2025 comments):

- a trivial page: **~352–378 MB**;
- a real site (servo.org): **400+ MB**; multiprocess mode is higher.

So Servo's *baseline* is ~20× our entire adblock budget (<15 MB — that design
goal is comfortably met and is not the problem). On a Pinebook Pro running sway
plus a terminal, expect **one or two tabs**, not ten. Multiprocess mode is a
default cargo feature; on 4 GB it is worth benchmarking single-process vs
multiprocess before assuming more processes is better.

Manage expectations: this is a browser for *focused* use on this machine, not for
keeping 20 tabs warm.

---

## 6. Security: be honest about what this is

The embedding path has no sandbox and no site isolation. For Booking.com — which
handles payment cards and personal data — that is a real consideration, not a
theoretical one. Also note Servo's own framing:

> The main goal of the project is not to build an end user browser product, but
> an embeddable web runtime.

serf is a thin, honest shell over an engine that is explicit about not being a
consumer browser. Daily-driving logins in it is a deliberate risk decision.

---

## 7. Suggested order of work

**P0 — make it a browser at all (our code, days not weeks)**
1. `WebViewDelegate` with `notify_new_frame_ready` → `paint()` + `present()`.
2. Forward winit input → `notify_input_event` (`Keyboard`, `MouseButton`,
   `MouseMove`, `Wheel`, `Touch`, `Ime`).
3. Scroll via `notify_scroll_event`, drop the JS hack.
4. Wire `notify_url_changed` / `notify_page_title_changed` /
   `notify_status_text_changed` / `notify_cursor_changed` / `notify_load_status_changed`
   into the existing overlay (the status bar already exists — it just has no feed).
5. `notify_input_event_handled` → vim keymap chains on unconsumed keys.

**P1 — browsable (Booking.com, logins, forms)**
6. `show_embedder_control` (form widgets, file upload), `request_create_new`
   (popups), `request_navigation` (beforeunload), `request_permission`.
7. Persistent cookies / localStorage / IndexedDB across restarts.
8. `ClipboardDelegate` for copy/paste; `notify_crashed`; `show_console_message`.
9. Fullscreen for video.

**P2 — media (accept partial)**
10. `--features media-gstreamer` + GStreamer + codecs + RK3399 V4L2 decode.
    Expect plain `<video>` to work and YouTube to be unreliable pending upstream MSE.

**P3 — WebGPU**
11. **Do not attempt.** No Vulkan driver for Mali-T860. Revisit only if wgpu's
    GLES backend becomes a supported Servo path.

---

## 8. References worth reading before writing code

- `doc.servo.org/servo/` — the embedding contract (steps 1–5).
- `WebViewDelegate` rustdoc — the full callback list with doc comments.
- `servo/servo` PR #39720 + "October in Servo" blog — the
  `notify_input_event_handled` design and why key chaining works that way.
- **Verso** (`versotile-org/verso`) — a Servo-based *browser* doing much of this;
  read it rather than inventing the delegate wiring.
- `webbeef/servo` `linux-arm64` branch — someone running Servo on a Pinephone Pro.
- Mesa Panfrost docs — the GPU capability table that rules out WebGPU here.
