//! End-to-end test of the control path: a real UNIX socket, the real
//! `serf-msg` client, the real session model — exercising exactly what
//! `main.rs`'s `serve_command` does, minus the window.
//!
//! This is the test that would catch a protocol drift between `serf-msg` and
//! the session dispatcher (the two halves that must agree on the command
//! vocabulary).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serf::browser::navigation::{Effect, Session};
use serf::ipc::{request, Response, Server};

fn tmp_sock(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "serf-it-{tag}-{}-{n}.sock",
        std::process::id()
    ))
}

/// Stand up a server bound to a real session, mirroring main.rs.
fn spawn_server(tag: &str) -> (PathBuf, Arc<Mutex<Vec<Effect>>>) {
    let path = tmp_sock(tag);
    let session = Arc::new(Mutex::new(Session::new("about:blank")));
    let effects: Arc<Mutex<Vec<Effect>>> = Arc::new(Mutex::new(Vec::new()));

    let session2 = session.clone();
    let effects2 = effects.clone();
    let server = Server::bind(&path).expect("bind control socket");
    let _handle = server.serve(move |line| {
        let (response, effect) = session2.lock().unwrap().handle_ipc(line);
        effects2.lock().unwrap().push(effect);
        response
    });
    // Keep the server alive by leaking the handle: the test process ends anyway.
    std::mem::forget(_handle);
    (path, effects)
}

#[test]
fn a_client_can_drive_the_browser_over_the_real_socket() {
    let (path, effects) = spawn_server("drive");

    // 1. open a URL
    let r: Response = request(&path, "open https://example.com/").expect("open");
    assert!(r.ok, "{r:?}");
    assert!(r.message.contains("example.com"));

    // 2. open a second tab
    let r = request(&path, "tab-new https://second.example/").expect("tab-new");
    assert!(r.ok, "{r:?}");

    // 3. ask for status and inspect the JSON
    let r = request(&path, "status").expect("status");
    assert!(r.ok);
    let data = r.data.expect("status carries data");
    assert_eq!(data["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(data["active"], 1);
    assert_eq!(data["tabs"][1]["url"], "https://second.example/");

    // 4. go back — must ask the engine to navigate
    let r = request(&path, "back").expect("back");
    assert!(r.ok);

    // 5. switch tabs and scroll
    assert!(request(&path, "tab-select 0").expect("select").ok);
    assert!(request(&path, "scroll 200").expect("scroll").ok);

    // 6. an unknown command must fail cleanly, not drop the connection
    let r = request(&path, "frobnicate").expect("unknown");
    assert!(!r.ok);
    assert!(r.message.contains("unknown command"));

    // 7. the channel still works afterwards
    assert!(request(&path, "status").expect("status again").ok);

    let effects = effects.lock().unwrap();
    assert!(
        effects.contains(&Effect::Navigate("https://example.com/".into())),
        "open must emit Navigate: {effects:?}"
    );
    assert!(
        effects.contains(&Effect::NewTab("https://second.example/".into())),
        "tab-new must emit NewTab: {effects:?}"
    );
    assert!(
        effects.contains(&Effect::Scroll { dx: 0.0, dy: 200.0 }),
        "scroll must emit Scroll: {effects:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn theme_and_quit_are_dispatched() {
    let (path, effects) = spawn_server("theme");

    let r = request(&path, "theme reload").expect("theme");
    assert!(r.ok);

    let r = request(&path, "quit").expect("quit");
    assert!(r.ok);

    let effects = effects.lock().unwrap();
    assert!(effects.contains(&Effect::ReloadTheme));
    assert!(effects.contains(&Effect::Quit));

    std::fs::remove_file(&path).ok();
}
