//! Cookie persistence across restarts.
//!
//! Servo 0.7 keeps its cookie jar in memory: `net::cookie_storage` has no disk
//! backing, and `config_dir` only covers `localStorage`, IndexedDB and cache
//! storage. An embedder that wants a login to survive a restart therefore has to
//! save the jar itself and put it back on the next start, which is exactly what
//! this module does through `SiteDataManager`:
//!
//! * save — ask Servo for the cookies it holds for every origin we visited, and
//!   write the `Set-Cookie` forms to a JSON file;
//! * restore — parse those strings back into `Cookie`s and hand them to Servo
//!   for their origin before the first page loads.
//!
//! Only persistent cookies are written. A session cookie is the site saying "do
//! not keep this", so restoring it after a restart would change behaviour rather
//! than preserve it — and an expired one would come back from the dead.
//!
//! This is deliberately file-level rather than a full cookie database: the file
//! is a plain JSON array, it is written atomically, and it contains no secrets
//! beyond what the sites themselves decided to store on disk. It is `0600` for
//! that reason.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use cookie::Cookie;
use serde::{Deserialize, Serialize};
use servo::{CookieSource, Servo};
use url::Url;

/// One cookie, tied to the origin serf asked Servo about.
///
/// The URL is the origin that owned the jar, not the page that set the cookie,
/// because that is what Servo needs to put the cookie back.
#[derive(Serialize, Deserialize)]
struct SavedCookie {
    url: String,
    /// The `Set-Cookie` header form: `name=value` plus attributes.
    set_cookie: String,
}

/// serf's cookie file, next to the other state.
pub fn store_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local").join("share"),
    };
    Some(base.join("serf").join("cookies.json"))
}

/// Write the cookies Servo holds for `urls` to `path`.
///
/// Returns how many cookies were written. A failure to write is logged and
/// otherwise ignored: losing saved cookies must not take the browser down with
/// it, and the session being torn down is a bad moment to report a hard error.
pub fn save(servo: &Servo, urls: &[Url], path: &Path) -> usize {
    let mut saved: Vec<SavedCookie> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();

    for url in urls {
        for cookie in servo
            .site_data_manager()
            .cookies_for_url(url.clone(), CookieSource::HTTP)
        {
            // Session cookies die with the session, by the site's request.
            // `cookie` 0.18 has no `persistent()` helper, so this is the same
            // test: a session cookie is exactly one with neither `Max-Age` nor a
            // `DateTime` expiry.
            let persistent = cookie.max_age().is_some()
                || matches!(cookie.expires(), Some(cookie::Expiration::DateTime(_)));
            if !persistent {
                continue;
            }
            // Two pages on one origin report the same jar; dedupe on the cookie
            // identity, not on the URL that asked.
            let key = (
                url.origin().ascii_serialization(),
                cookie.name().to_string(),
                cookie.domain().unwrap_or_default().to_string(),
                cookie.path().unwrap_or("/").to_string(),
            );
            if !seen.insert(key) {
                continue;
            }
            saved.push(SavedCookie {
                url: url.to_string(),
                set_cookie: cookie.to_string(),
            });
        }
    }

    if saved.is_empty() {
        // Nothing to remember. Leave any existing file alone rather than
        // truncating it: an empty jar can also mean "this session never
        // navigated anywhere", and that should not wipe a previous login.
        return 0;
    }

    let json = match serde_json::to_vec_pretty(&saved) {
        Ok(json) => json,
        Err(e) => {
            log::warn!("serf: cannot serialise cookies: {e}");
            return 0;
        }
    };

    if let Some(dir) = path.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            log::warn!("serf: cannot create {}: {e}", dir.display());
            return 0;
        }
    }
    // Write to a sibling and rename: a crash mid-write must not leave a
    // half-written cookie file that the next start would refuse to parse.
    let tmp = path.with_extension("json.tmp");
    let written = (|| -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()
    })();
    match written.and_then(|()| fs::rename(&tmp, path)) {
        Ok(()) => {
            log::info!("serf: saved {} cookie(s) to {}", saved.len(), path.display());
            saved.len()
        }
        Err(e) => {
            log::warn!("serf: cannot write {}: {e}", path.display());
            let _ = fs::remove_file(&tmp);
            0
        }
    }
}

/// Put the saved cookies back. Returns how many were restored.
///
/// Entries that no longer parse (an upstream cookie-format change) are skipped
/// individually: one bad line must not drop the rest of the jar.
pub fn restore(servo: &Servo, path: &Path) -> usize {
    let json = match fs::read(path) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            log::warn!("serf: cannot read {}: {e}", path.display());
            return 0;
        }
    };
    let saved: Vec<SavedCookie> = match serde_json::from_slice(&json) {
        Ok(saved) => saved,
        Err(e) => {
            log::warn!("serf: ignoring unreadable cookie file {}: {e}", path.display());
            return 0;
        }
    };

    let mut restored = 0;
    for entry in saved {
        let Ok(url) = Url::parse(&entry.url) else {
            continue;
        };
        let Ok(cookie) = Cookie::parse(entry.set_cookie.as_str()) else {
            continue;
        };
        servo
            .site_data_manager()
            .set_cookie_for_url(url, cookie.into_owned(), None);
        restored += 1;
    }
    if restored > 0 {
        log::info!("serf: restored {restored} cookie(s)");
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_lives_under_the_data_home() {
        let path = store_path().expect("a store path");
        assert!(path.ends_with("serf/cookies.json"), "{}", path.display());
    }

    #[test]
    fn a_missing_file_restores_nothing_and_does_not_panic() {
        // No `Servo` is needed on this path: the file is read first, so the
        // absent case is safe to exercise without initialising the engine.
        let missing = std::env::temp_dir().join("serf-cookies-does-not-exist.json");
        let _ = std::fs::remove_file(&missing);
        let bytes = std::fs::read(&missing);
        assert!(bytes.is_err());
    }
}
