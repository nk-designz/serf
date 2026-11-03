//! serf's [`WebViewDelegate`] — the embedder half of Servo.
//!
//! Servo calls these methods from its own threads, at times serf does not
//! control, so nothing here may touch the tab model, the overlay or any other
//! main-thread state. Each callback instead folds into [`DelegateState`], which
//! the winit event loop drains once per turn through the `Engine` trait:
//!
//! * page facts (URL, title, status, load, history, crash) → `Engine::take_page_state`
//! * `window.open` / `target="_blank"` → `Engine::take_new_tabs`
//! * a `<select>`, file, colour or context-menu control → `Engine::pending_control`
//! * a ready frame → `Engine::wants_repaint`
//!
//! The delegate observes; the event loop decides. That split is also what keeps
//! the vim keymap authoritative: keys reach the page only in INSERT mode, and
//! `Engine::last_key_handled` reports whether the page took them.
//!
//! Only the *main* web view drives the model. An auxiliary view built for
//! `window.open` reports its URL as a new tab and is otherwise ignored, so a
//! popup can never overwrite the address of the page you are reading.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use servo::{
    AllowOrDenyRequest, AuthenticationRequest, ConsoleLogLevel, ContextMenuAction,
    ContextMenuItem, CreateNewWebViewRequest, EmbedderControl, EmbedderControlId, InputEventId,
    InputEventResult, LoadStatus, NavigationRequest, PermissionRequest, RgbColor, SelectElement,
    SelectElementOptionOrOptgroup, SimpleDialog, WebResourceLoad, WebResourceResponse, WebView,
    WebViewDelegate, WebViewId,
};
use crate::adblock::RequestKind;
// `Url` is the `url` crate's, not servo's — servo does not re-export it, and
// `WebResourceRequest.url` and the navigation callbacks are all `url::Url`.
// serf already depends on `url` for parsing, so this is the same instance.
use url::Url;

use super::engine::{ControlChoice, ControlRequest, PageState};

/// How many `window.open` web views serf keeps alive before it starts ignoring
/// them. Each one holds a whole page, so this is a memory guard on a 4 GB box,
/// not a feature limit.
const MAX_AUX_WEBVIEWS: usize = 8;

/// Everything the delegate has learned that the event loop has not consumed yet.
///
/// Held behind an `Rc` because Servo keeps the delegate alive for the life of
/// the web view — longer than the call that created it.
#[derive(Default)]
pub struct DelegateState {
    /// The web view whose callbacks drive serf's tab model. Nothing is reported
    /// until this is set (from `ServoEngine::create_webview`).
    main: RefCell<Option<WebViewId>>,
    inner: RefCell<Inner>,
}

#[derive(Default)]
struct Inner {
    state: PageState,
    /// URLs the page asked to open elsewhere, oldest first.
    new_tabs: VecDeque<String>,
    /// The control the page is blocked on, if any.
    control: Option<PendingControl>,
    /// Auxiliary web views kept alive so the page's `window.open` handle stays
    /// valid.
    aux: Vec<WebView>,
    /// A new frame is ready to paint.
    dirty: bool,
    /// Whether the page consumed the last key serf forwarded.
    key_handled: Option<bool>,
}

/// A form control the page is blocked on: the live Servo object that holds the
/// responder, plus the neutral description the event loop sees.
struct PendingControl {
    control: EmbedderControl,
    request: ControlRequest,
    /// For a context menu, the action each visible label maps to. Labels and
    /// actions are parallel, so a disabled or separator item is simply absent
    /// from both.
    menu_actions: Vec<ContextMenuAction>,
}

// --- the event loop's view of the delegate ---------------------------------

impl DelegateState {
    /// Point the delegate at the web view that drives the tab model.
    pub fn set_main(&self, id: WebViewId) {
        *self.main.borrow_mut() = Some(id);
    }

    /// Everything the page reported since the last call, or `None` if nothing
    /// did.
    pub fn take_state(&self) -> Option<PageState> {
        let mut inner = self.inner.borrow_mut();
        let state = std::mem::take(&mut inner.state);
        (state != PageState::default()).then_some(state)
    }

    /// The URLs the page asked to open elsewhere, draining the queue.
    pub fn take_new_tabs(&self) -> Vec<String> {
        self.inner.borrow_mut().new_tabs.drain(..).collect()
    }

    /// The control the page is blocked on, if any.
    pub fn pending_control(&self) -> Option<ControlRequest> {
        self.inner
            .borrow()
            .control
            .as_ref()
            .map(|pending| pending.request.clone())
    }

    /// Answer [`DelegateState::pending_control`], unblocking the page.
    pub fn resolve_control(&self, choice: ControlChoice) {
        let Some(pending) = self.inner.borrow_mut().control.take() else {
            return;
        };
        answer_control(pending, choice);
    }

    /// Whether the page is waiting on a control, in which case keys belong to
    /// the control and must not be forwarded to the document.
    pub fn has_pending_control(&self) -> bool {
        self.inner.borrow().control.is_some()
    }

    /// Whether a frame is ready to paint. Clears the flag.
    pub fn take_repaint(&self) -> bool {
        std::mem::replace(&mut self.inner.borrow_mut().dirty, false)
    }

    /// Whether the page consumed the last key serf forwarded, if a verdict has
    /// arrived yet.
    pub fn last_key_handled(&self) -> Option<bool> {
        self.inner.borrow().key_handled
    }

    // Recorders. These are what the callbacks below delegate to, so the
    // accumulation rules are testable without a live Servo.

    fn note_url(&self, url: String) {
        self.inner.borrow_mut().state.url = Some(url);
    }

    fn note_title(&self, title: Option<String>) {
        // A page with no `<title>` is reported as an empty one: the overlay
        // falls back to the URL for either.
        self.inner.borrow_mut().state.title = Some(title.unwrap_or_default());
    }

    fn note_status(&self, status: Option<String>) {
        // Wrapped twice on purpose: the outer layer is "the page said
        // something", the inner one is the text, which may be "clear it".
        self.inner.borrow_mut().state.status = Some(status);
    }

    fn note_loading(&self, loading: bool) {
        self.inner.borrow_mut().state.loading = Some(loading);
    }

    fn note_history(&self) {
        self.inner.borrow_mut().state.history = Some(true);
    }

    fn note_crashed(&self, reason: String) {
        self.inner.borrow_mut().state.crashed = Some(reason);
    }

    fn note_fullscreen(&self, on: bool) {
        self.inner.borrow_mut().state.fullscreen = Some(on);
    }

    fn note_console(&self, line: String) {
        self.inner.borrow_mut().state.console.push(line);
    }

    fn note_frame_ready(&self) {
        self.inner.borrow_mut().dirty = true;
    }

    fn note_new_tab(&self, url: String) {
        self.inner.borrow_mut().new_tabs.push_back(url);
    }

    fn note_key_handled(&self, handled: bool) {
        self.inner.borrow_mut().key_handled = Some(handled);
    }

    fn note_control(&self, control: EmbedderControl, request: ControlRequest, menu_actions: Vec<ContextMenuAction>) {
        let mut inner = self.inner.borrow_mut();
        if inner.control.is_some() {
            // Only one control can be pending: adopting a second would strand
            // the first one's responder and leave that element blocked forever.
            // Dropping the new control dismisses it.
            log::warn!("serf: a form control arrived while another was pending; dismissing the new one");
            return;
        }
        inner.control = Some(PendingControl {
            control,
            request,
            menu_actions,
        });
    }

    fn forget_control(&self, id: EmbedderControlId) {
        let mut inner = self.inner.borrow_mut();
        if inner.control.as_ref().map(|p| p.control.id()) == Some(id) {
            // The page closed the control itself; dropping it is the dismissal.
            inner.control = None;
        }
    }

    fn has_room_for_aux(&self) -> bool {
        self.inner.borrow().aux.len() < MAX_AUX_WEBVIEWS
    }

    /// Keep an auxiliary web view alive for the page's `window.open` handle.
    fn adopt_aux(&self, webview: WebView) {
        self.inner.borrow_mut().aux.push(webview);
    }
}

// --- the delegate ----------------------------------------------------------

/// The [`WebViewDelegate`] serf installs on every web view it creates.
pub struct SerfDelegate {
    state: Rc<DelegateState>,
    /// The compiled filter engine. `adblock::Engine` is `!Send` and lives on the
    /// main thread; Servo delivers `load_web_resource` on the thread that drives
    /// `spin_event_loop`, so this is reachable exactly where it is needed.
    adblock: Option<Rc<crate::adblock::AdblockEngine>>,
}

impl SerfDelegate {
    pub fn new(state: Rc<DelegateState>, adblock: Option<Rc<crate::adblock::AdblockEngine>>) -> Self {
        Self { state, adblock }
    }

    /// Whether `webview` is the one that drives the tab model.
    fn is_main(&self, webview: &WebView) -> bool {
        *self.state.main.borrow() == Some(webview.id())
    }

    /// The network hook: cancel a request the filter lists reject.
    ///
    /// Dropping the load without intercepting it means "let it through", which is
    /// what Servo's default responder does, so only a positive block decision
    /// costs anything. Cosmetic filtering (`cosmetic_css`) hides what is left.
    fn block_if_filtered(&self, webview: &WebView, load: WebResourceLoad) {
        let Some(adblock) = self.adblock.as_ref() else { return };
        // Everything the decision needs is copied out first: `load` is consumed
        // by `intercept`, and `request` borrows it.
        let (url, decision) = {
            let request = load.request();
            let source = request
                .referrer_url
                .as_ref()
                .map(|url| url.to_string())
                .or_else(|| webview.url().map(|url| url.to_string()))
                .unwrap_or_default();
            let kind = RequestKind::from_fetch_dest(
                request
                    .headers
                    .get("sec-fetch-dest")
                    .and_then(|v| v.to_str().ok()),
                request.url.as_str(),
            );
            let decision = adblock.check(
                request.url.as_str(),
                &source,
                kind,
                request.method.as_str(),
            );
            if decision.is_blocking() {
                log::debug!(
                    "serf: blocked {} ({}) for {}",
                    request.url,
                    kind.as_str(),
                    source
                );
            }
            (request.url.clone(), decision)
        };
        if !decision.is_blocking() {
            return;
        }
        load.intercept(WebResourceResponse::new(url)).cancel();
    }
}

impl WebViewDelegate for SerfDelegate {
    fn notify_url_changed(&self, webview: WebView, url: Url) {
        if self.is_main(&webview) {
            self.state.note_url(url.to_string());
        } else {
            // An auxiliary view landed somewhere: that is a new tab for serf,
            // which renders everything in its single main view.
            self.state.note_new_tab(url.to_string());
        }
    }

    /// The filter decision is `Decision`; only the blocking variants cancel.
    fn load_web_resource(&self, webview: WebView, load: WebResourceLoad) {
        self.block_if_filtered(&webview, load);
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        if self.is_main(&webview) {
            self.state.note_title(title);
        }
    }

    fn notify_status_text_changed(&self, webview: WebView, status: Option<String>) {
        if self.is_main(&webview) {
            self.state.note_status(status);
        }
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if !self.is_main(&webview) {
            return;
        }
        self.state.note_loading(match status {
            LoadStatus::Started | LoadStatus::HeadParsed => true,
            LoadStatus::Complete => false,
        });
    }

    fn notify_history_changed(&self, webview: WebView, _entries: Vec<Url>, _current: usize) {
        if self.is_main(&webview) {
            self.state.note_history();
        }
    }

    fn notify_new_frame_ready(&self, _webview: WebView) {
        // Frames from auxiliary views are never presented, so this is not gated
        // on `is_main`: asking for a paint is harmless, and a missed one would
        // freeze the visible page.
        self.state.note_frame_ready();
    }

    fn notify_input_event_handled(
        &self,
        webview: WebView,
        _event_id: InputEventId,
        result: InputEventResult,
    ) {
        if !self.is_main(&webview) {
            return;
        }
        // `Consumed` means Servo's own handler took the event — a key typed into
        // a focused `<input>`, say. That is the signal that a keybinding must not
        // also fire, which is what makes a modal keymap safe in INSERT mode.
        let handled = result.contains(InputEventResult::Consumed)
            || result.contains(InputEventResult::DefaultPrevented);
        self.state.note_key_handled(handled);
    }

    fn notify_crashed(&self, webview: WebView, reason: String, backtrace: Option<String>) {
        if !self.is_main(&webview) {
            return;
        }
        log::error!("serf: the content process crashed: {reason}");
        if let Some(backtrace) = backtrace {
            log::debug!("serf: crash backtrace:\n{backtrace}");
        }
        self.state.note_crashed(reason);
    }

    fn notify_fullscreen_state_changed(&self, webview: WebView, is_fullscreen: bool) {
        if self.is_main(&webview) {
            self.state.note_fullscreen(is_fullscreen);
        }
    }

    fn notify_closed(&self, webview: WebView) {
        // A page calling `window.close()`: serf's tab is model-only, so the
        // overlay is told and the window stays. Recorded so the status line can
        // say so rather than looking frozen.
        if self.is_main(&webview) {
            self.state.note_console("page requested close".to_string());
        }
    }

    fn show_console_message(&self, webview: WebView, level: ConsoleLogLevel, message: String) {
        if !self.is_main(&webview) {
            return;
        }
        let level = match level {
            ConsoleLogLevel::Error => "error",
            ConsoleLogLevel::Warn => "warn",
            ConsoleLogLevel::Info => "info",
            ConsoleLogLevel::Debug => "debug",
            ConsoleLogLevel::Trace => "trace",
            ConsoleLogLevel::Dir => "dir",
            ConsoleLogLevel::Log => "log",
        };
        self.state.note_console(format!("{level}: {message}"));
    }

    fn request_navigation(&self, _webview: WebView, navigation_request: NavigationRequest) {
        // serf has no modal UI to ask with, and browsers do not prompt for
        // ordinary navigations either. `mailto:` and friends never reach here —
        // Servo routes those through `request_protocol_handler`, whose default
        // (deny) serf keeps.
        navigation_request.allow();
    }

    fn request_unload(&self, _webview: WebView, unload_request: AllowOrDenyRequest) {
        // Servo's own default is to allow. serf has no "leave site?" prompt, and
        // a browser that silently stops navigating because a page set
        // `onbeforeunload` is far worse than one that loses a form.
        unload_request.allow();
    }

    fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
        if !self.is_main(&parent_webview) {
            // No popups from popups.
            return;
        }
        if !self.state.has_room_for_aux() {
            log::warn!(
                "serf: ignoring window.open — already holding {MAX_AUX_WEBVIEWS} auxiliary views"
            );
            // Dropping the request means no new view is opened, which is the
            // documented behaviour for an ignored request.
            return;
        }
        // Build the child so the page's `window.open` handle is real: sites
        // routinely call `win.focus()` or poll `win.closed`, and dropping the
        // request outright breaks them. The child is never painted, so it cannot
        // draw over the main view; its URL arrives as a new tab through
        // `notify_url_changed`.
        //
        // The delegate is shared, so this must not hold a borrow across `build()`
        // — a synchronous callback would re-enter and panic.
        let child = request
            .builder(parent_webview.rendering_context())
            // The popup gets the same filter so an ad in a `window.open` is not
            // a way around the blocklist.
            .delegate(Rc::new(SerfDelegate::new(
                Rc::clone(&self.state),
                self.adblock.clone(),
            )))
            .build();
        self.state.adopt_aux(child);
    }

    fn request_permission(&self, _webview: WebView, request: PermissionRequest) {
        // `geolocation`, `notifications`, camera, microphone ... serf has no
        // prompt, so the safe default applies: deny, and say so in the log.
        log::info!("serf: denying the {:?} permission request", request.feature());
        request.deny();
    }

    fn request_authentication(&self, _webview: WebView, request: AuthenticationRequest) {
        // serf never collects credentials: there is no prompt that can be
        // trusted to keep them, and a browser that silently stashes an HTTP auth
        // pair is a liability. Dropping the request cancels the authentication,
        // which is what the page sees as a 401.
        log::info!(
            "serf: refusing to prompt for HTTP auth on {} (dropping the request cancels it)",
            request.url()
        );
    }

    fn show_embedder_control(&self, webview: WebView, embedder_control: EmbedderControl) {
        if !self.is_main(&webview) {
            return;
        }

        // Simple dialogs are the one control serf answers immediately. There is
        // no modal UI to block on, and an unanswered `alert()` freezes the page
        // on Servo's side, so waiting for the user is not an option yet. They are
        // logged rather than hidden, and answered the safe way: OK for an alert,
        // "cancel" for a confirm or a prompt.
        if let EmbedderControl::SimpleDialog(dialog) = embedder_control {
            log::info!("serf: page dialog: {}", dialog.message());
            match dialog {
                SimpleDialog::Alert(alert) => alert.confirm(),
                SimpleDialog::Confirm(confirm) => confirm.dismiss(),
                SimpleDialog::Prompt(prompt) => prompt.dismiss(),
            }
            return;
        }

        // Everything else renders as an external menu, so the request is
        // described in serf's own vocabulary and handed to the event loop.
        let (request, menu_actions) = match &embedder_control {
            EmbedderControl::SelectElement(select) => (
                ControlRequest::Select {
                    options: option_labels(select),
                    size: None,
                },
                Vec::new(),
            ),
            EmbedderControl::ColorPicker(picker) => (
                ControlRequest::Color {
                    current: picker.current_color().map(|c| (c.red, c.green, c.blue)),
                },
                Vec::new(),
            ),
            EmbedderControl::FilePicker(picker) => (
                ControlRequest::File {
                    multiple: picker.allow_select_multiple(),
                },
                Vec::new(),
            ),
            EmbedderControl::ContextMenu(menu) => {
                let mut labels = Vec::new();
                let mut actions = Vec::new();
                for item in menu.items() {
                    // Separators and disabled entries are not offered, and are
                    // left out of both lists so the indices stay parallel.
                    if let ContextMenuItem::Item {
                        label,
                        action,
                        enabled: true,
                    } = item
                    {
                        labels.push(label.clone());
                        actions.push(*action);
                    }
                }
                (ControlRequest::ContextMenu { items: labels }, actions)
            }
            // An IME request is a notification rather than a question, and serf
            // has no virtual keyboard: dropping it leaves the page unblocked.
            EmbedderControl::InputMethod(_) => return,
            // Answered above.
            EmbedderControl::SimpleDialog(_) => return,
        };

        self.state.note_control(embedder_control, request, menu_actions);
    }

    fn hide_embedder_control(&self, webview: WebView, control_id: EmbedderControlId) {
        if self.is_main(&webview) {
            self.state.forget_control(control_id);
        }
    }
}

// --- answering a control ---------------------------------------------------

/// Resolve a pending control, preferring the user's answer.
fn answer_control(pending: PendingControl, choice: ControlChoice) {
    match (pending.control, choice) {
        (EmbedderControl::SelectElement(mut select), ControlChoice::Selected(indices)) => {
            // `select` takes indices into the same mixed option/optgroup list the
            // labels were built from. Out-of-range indices are dropped rather
            // than clamped: Servo asserts nothing, but a wrong index would select
            // a different item than the one the user picked.
            let count = select.options().len();
            let chosen = indices
                .into_iter()
                .map(|i| i as usize)
                .filter(|i| *i < count)
                .collect();
            select.select(chosen);
            select.submit();
        }
        (EmbedderControl::ColorPicker(mut picker), ControlChoice::Color(hex)) => {
            picker.select(parse_hex(&hex));
            picker.submit();
        }
        (EmbedderControl::FilePicker(mut picker), ControlChoice::Files(paths)) => {
            let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
            picker.select(&paths);
            picker.submit();
        }
        (EmbedderControl::ContextMenu(menu), ControlChoice::MenuItem(index)) => {
            match pending.menu_actions.get(index as usize) {
                Some(action) => menu.select(*action),
                // The caller picked a row that is not in the menu it was shown:
                // dismissing is the only honest answer.
                None => menu.dismiss(),
            }
        }
        // A dismissal, or an answer that does not fit the control that was
        // pending: close it either way so the element is never left blocked.
        (control, _) => dismiss_control(control),
    }
}

/// Close a control without choosing anything.
fn dismiss_control(control: EmbedderControl) {
    match control {
        EmbedderControl::SelectElement(mut select) => {
            select.select(Vec::new());
            select.submit();
        }
        EmbedderControl::ColorPicker(mut picker) => {
            picker.select(None);
            picker.submit();
        }
        EmbedderControl::FilePicker(picker) => picker.dismiss(),
        EmbedderControl::ContextMenu(menu) => menu.dismiss(),
        // Neither blocks the page, so dropping is enough.
        EmbedderControl::InputMethod(_) | EmbedderControl::SimpleDialog(_) => {}
    }
}

/// The labels of a `<select>`, one per index in [`SelectElement::options`].
///
/// `options()` mixes `<option>` and `<optgroup>`, and selection indices address
/// that same mixed list — so an `<optgroup>` has to occupy its own slot here
/// rather than being flattened away, or every index after one would be wrong.
fn option_labels(select: &SelectElement) -> Vec<String> {
    select
        .options()
        .iter()
        .map(|entry| match entry {
            SelectElementOptionOrOptgroup::Option(option) => option.label.clone(),
            SelectElementOptionOrOptgroup::Optgroup { label, .. } => format!("— {label} —"),
        })
        .collect()
}

/// Parse `#rrggbb` (with or without the `#`) into a Servo colour.
fn parse_hex(text: &str) -> Option<RgbColor> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(RgbColor {
        red: ((value >> 16) & 0xff) as u8,
        green: ((value >> 8) & 0xff) as u8,
        blue: (value & 0xff) as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_delegate_reports_nothing() {
        let state = DelegateState::default();
        assert_eq!(state.take_state(), None);
        assert!(state.take_new_tabs().is_empty());
        assert!(state.pending_control().is_none());
        assert!(!state.take_repaint());
        assert_eq!(state.last_key_handled(), None);
    }

    #[test]
    fn page_facts_accumulate_into_one_snapshot() {
        let state = DelegateState::default();
        state.note_url("https://example.com/".into());
        state.note_title(Some("Example".into()));
        state.note_loading(true);
        state.note_status(Some("loading...".into()));

        let snapshot = state.take_state().expect("something was reported");
        assert_eq!(snapshot.url.as_deref(), Some("https://example.com/"));
        assert_eq!(snapshot.title.as_deref(), Some("Example"));
        assert_eq!(snapshot.loading, Some(true));
        assert_eq!(snapshot.status, Some(Some("loading...".into())));
        // Drained: the next drain has nothing.
        assert_eq!(state.take_state(), None);
    }

    #[test]
    fn a_cleared_status_is_distinct_from_no_news() {
        let state = DelegateState::default();
        state.note_status(None);
        let snapshot = state.take_state().expect("a clear is news");
        assert_eq!(snapshot.status, Some(None));
    }

    #[test]
    fn a_titleless_page_reports_an_empty_title_not_none() {
        let state = DelegateState::default();
        state.note_title(None);
        let snapshot = state.take_state().expect("a title change is news");
        assert_eq!(snapshot.title.as_deref(), Some(""));
    }

    #[test]
    fn console_messages_keep_the_whole_burst() {
        let state = DelegateState::default();
        state.note_console("log: one".into());
        state.note_console("error: two".into());
        let snapshot = state.take_state().expect("console output is news");
        assert_eq!(snapshot.console, vec!["log: one", "error: two"]);
    }

    #[test]
    fn new_tabs_drain_in_order() {
        let state = DelegateState::default();
        state.note_new_tab("https://a.example/".into());
        state.note_new_tab("https://b.example/".into());
        assert_eq!(
            state.take_new_tabs(),
            vec!["https://a.example/", "https://b.example/"]
        );
        assert!(state.take_new_tabs().is_empty());
    }

    #[test]
    fn a_ready_frame_is_reported_once() {
        let state = DelegateState::default();
        state.note_frame_ready();
        assert!(state.take_repaint());
        assert!(!state.take_repaint());
    }

    #[test]
    fn resolving_with_nothing_pending_is_a_no_op() {
        let state = DelegateState::default();
        state.resolve_control(ControlChoice::Dismissed);
        assert!(state.pending_control().is_none());
    }

    /// `servo::RgbColor` is a foreign type that does not implement `PartialEq`
    /// (servo-embedder-traits-0.7.0/lib.rs:1112), so compare its channels rather
    /// than the struct.
    fn rgb(c: RgbColor) -> (u8, u8, u8) {
        (c.red, c.green, c.blue)
    }

    #[test]
    fn parse_hex_accepts_the_forms_a_colour_input_gets() {
        let expected = Some((0xcf, 0x6a, 0x4c));
        assert_eq!(parse_hex("#cf6a4c").map(rgb), expected);
        assert_eq!(parse_hex("cf6a4c").map(rgb), expected);
        assert_eq!(parse_hex("  #CF6A4C  ").map(rgb), expected);
    }

    #[test]
    fn parse_hex_rejects_anything_else() {
        for bad in ["", "#", "#fff", "cf6a4", "cf6a4cc", "#gggggg", "not a colour"] {
            assert_eq!(parse_hex(bad).map(rgb), None, "{bad:?} should not parse");
        }
    }
}
