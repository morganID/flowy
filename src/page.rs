//! Safari plumbing: the in-process extension bridge, one pinned engine, and the one
//! flow.google.com tab every command reuses.
//!
//! Nothing here touches the Flow UI; this module only guarantees that there is a
//! live, loaded Flow page with an owned tab receipt behind it. The DOM work lives
//! in [`crate::ui`].

use anyhow::{Result, bail};
use safari_driver::{AutoBackend, SafariEngine};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
    time::{Duration, Instant},
};

/// The Flow app. Every command drives this one page.
pub const FLOW_URL: &str = "https://flow.google.com/";
const FLOW_HOST: &str = "flow.google.com";
const PAGE_TIMEOUT: Duration = Duration::from_secs(45);

/// The one engine for this process. A pinned session id lets the extension keep the
/// tab this user already owns, so a later process can adopt it instead of opening
/// another one.
pub fn engine() -> &'static SafariEngine {
    static ENGINE: OnceLock<SafariEngine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        SafariEngine::with_backend(AutoBackend::with_session(
            crate::browser::BrowserState::load().session_id,
            None,
        ))
    })
}

/// Run a synchronous page script and decode its JSON value.
pub fn eval(engine: &SafariEngine, script: &str) -> Result<Value> {
    engine
        .evaluate_json::<Value>(script)
        .map_err(|error| anyhow::anyhow!("Safari page evaluation failed: {error}"))
}

/// Navigate the owned tab and let the new document settle.
pub fn navigate(engine: &SafariEngine, url: &str) -> Result<()> {
    engine
        .execute("safari_navigate", &json!({"url": url}))
        .map_err(|error| {
            anyhow::anyhow!("Could not open {url} in the owned Safari tab: {error}")
        })?;
    Ok(())
}

/// Keep one owned Flow tab per Safari session and reuse it for every request: the
/// tab this process already drives, then a tab the session opened earlier, and only
/// when neither exists a new tab.
pub fn ensure_tab(engine: &SafariEngine) -> Result<()> {
    static TAB_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = TAB_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("Flow tab initialization lock was poisoned"))?;

    if engine.current_receipt().is_some() {
        match eval(engine, "location.hostname") {
            Ok(Value::String(host)) if is_flow_host(&host) => {
                return wait_for_page(engine);
            }
            // An owned tab parked on another page is steered back instead of duplicated.
            Ok(Value::String(_)) => {
                navigate(engine, FLOW_URL)?;
                return wait_for_page(engine);
            }
            // The stored receipt no longer resolves: try to adopt a tab below.
            _ => {}
        }
    }

    if adopt_tab(engine) {
        return wait_for_page(engine);
    }

    engine
        .execute("safari_new_tab", &json!({"url": FLOW_URL}))
        .map_err(|error| anyhow::anyhow!("Safari extension bridge cannot open a Flow tab: {error}. Check that the selected Safari profile has its extension enabled."))?;
    wait_for_page(engine)
}

/// Take over a Flow tab this Safari session already opened in an earlier process.
/// `safari_switch_tab` only adopts tabs the session owns, so a tab the user opened by
/// hand is refused by the extension and a new tab is opened instead.
fn adopt_tab(engine: &SafariEngine) -> bool {
    if engine.backend_name() != "safari-extension" {
        eprintln!("flowy: no tab to adopt (backend is {})", engine.backend_name());
        return false;
    }
    let tabs = match engine.execute("safari_list_tabs", &json!({})) {
        Ok(tabs) => tabs,
        Err(error) => {
            eprintln!("flowy: no tab to adopt (listing tabs failed: {error})");
            return false;
        }
    };
    let Some(tab_index) = flow_tab_index(&tabs) else {
        eprintln!("flowy: no tab to adopt (no flow.google.com tab is open)");
        return false;
    };
    match engine.execute(
        "safari_switch_tab",
        &json!({"tabIndex": tab_index, "allow_user_tab": true}),
    ) {
        Ok(_) if engine.current_receipt().is_some() => true,
        Ok(_) => {
            eprintln!("flowy: no tab to adopt (switched to tab {tab_index} but got no receipt)");
            false
        }
        Err(error) => {
            eprintln!("flowy: no tab to adopt (switching to tab {tab_index} failed: {error})");
            false
        }
    }
}

/// Find the 1-based index of a Flow tab. The engine maps the extension's index into
/// `window_id`; the raw extension payload names it `index`.
fn flow_tab_index(tabs: &Value) -> Option<i64> {
    tabs.as_array()?.iter().find_map(|tab| {
        let url = tab
            .get("url")
            .or_else(|| tab.get("safeUrl"))
            .and_then(Value::as_str)?;
        if !is_flow_url(url) {
            return None;
        }
        tab.get("index")
            .and_then(Value::as_i64)
            .or_else(|| tab.get("window_id").and_then(Value::as_str)?.parse().ok())
            .filter(|index| *index >= 1)
    })
}

fn is_flow_host(host: &str) -> bool {
    host.eq_ignore_ascii_case(FLOW_HOST)
}

fn is_flow_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    is_flow_host(host)
}

/// Start the Safari bridge inside this process. safari-driver's default library
/// constructor otherwise relaunches current_exe() with `serve`, which is wrong
/// when current_exe is a consumer binary such as flowy.
pub fn ensure_bridge() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            let address = SocketAddr::from(([127, 0, 0, 1], 9240));
            if bridge_responds(&address) {
                return Ok(());
            }
            let (tx, rx) = std::sync::mpsc::channel();
            thread::Builder::new()
                .name("safari-driver-bridge".into())
                .spawn(move || {
                    let result = safari_driver::bridge_host::serve();
                    let _ = tx.send(result);
                })
                .map_err(|error| format!("cannot launch Safari bridge thread: {error}"))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if bridge_responds(&address) {
                    return Ok(());
                }
                if let Ok(Err(error)) = rx.try_recv() {
                    return Err(error);
                }
                if Instant::now() >= deadline {
                    return Err("Safari bridge did not start on 127.0.0.1:9240; this process may be sandboxed from localhost networking".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
        .clone()
        .map_err(|error| anyhow::anyhow!(error))
}

fn bridge_responds(address: &SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(address, Duration::from_millis(100)) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .ok();
    if stream
        .write_all(b"GET /_safari_driver HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok() && response.contains("safari-driver-rust")
}

/// Wait until the owned tab is on flow.google.com with a parsed document. The React app
/// needs a moment longer, which [`crate::ui::wait_for_composer`] covers.
pub fn wait_for_page(engine: &SafariEngine) -> Result<()> {
    let deadline = Instant::now() + PAGE_TIMEOUT;
    let mut last_state = Value::Null;
    let mut last_error = None;
    while Instant::now() < deadline {
        match eval(
            engine,
            "({hostname:location.hostname,href:location.origin+location.pathname,readyState:document.readyState,title:document.title})",
        ) {
            Ok(state) => {
                let ready = state
                    .get("hostname")
                    .and_then(Value::as_str)
                    .is_some_and(is_flow_host)
                    && state
                        .get("readyState")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value != "loading");
                last_state = state;
                if ready {
                    return Ok(());
                }
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        thread::sleep(Duration::from_millis(300));
    }
    let url = last_state
        .get("href")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let title = last_state
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let state = last_state
        .get("readyState")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let evaluation = last_error
        .map(|error| format!("; last page evaluation error: {error}"))
        .unwrap_or_default();
    bail!(
        "Safari tab never reached the Flow app (url: {url}, title: {title}, document: {state}{evaluation}). Verify the extension opened the signed-in profile and that flow.google.com finished loading"
    )
}

/// Bring the owned tab to the front.
///
/// This is the fallback for a tab the background patch cannot wake up: it selects the tab
/// and activates Safari, because `document.hidden` stays true until both are done.
pub fn focus_tab(engine: &SafariEngine) -> Result<()> {
    if let Ok(tabs) = engine.execute("safari_list_tabs", &json!({}))
        && let Some(tab_index) = flow_tab_index(&tabs)
    {
        let _ = engine.execute(
            "safari_switch_tab",
            &json!({"tabIndex": tab_index, "allow_user_tab": true}),
        );
    }
    let _ = std::process::Command::new("open")
        .arg("-a")
        .arg("Safari")
        .status();
    Ok(())
}

/// Wait until the page reports itself visible, so rendering is actually running.
pub fn wait_until_visible(engine: &SafariEngine, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Value::Bool(false)) = eval(engine, "document.hidden") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "the Flow tab is still in the background, so this could not bring it forward. Rendering does not depend on it — flowy pumps frames itself — but a visible tab is faster"
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Serialize callers: the Flow tab can only hold one turn at a time, so parallel
/// requests would otherwise type into the same composer.
pub fn serialize_requests() -> Result<MutexGuard<'static, ()>> {
    static REQUEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    REQUEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("Flow request lock was poisoned"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_flow_tab_in_an_engine_tab_list() {
        let tabs = json!([
            {"window_id":"1","title":"Start Page","url":"https://example.com/"},
            {"window_id":"2","title":"Flow","url":"https://flow.google.com/project/6f2c"},
        ]);
        assert_eq!(flow_tab_index(&tabs), Some(2));
    }

    #[test]
    fn finds_the_flow_tab_in_a_raw_extension_tab_list() {
        let tabs = json!([
            {"index":1,"title":"Flow","safeUrl":"https://flow.google.com/","active":false},
        ]);
        assert_eq!(flow_tab_index(&tabs), Some(1));
        assert_eq!(flow_tab_index(&json!([])), None);
    }

    #[test]
    fn matches_only_the_flow_host() {
        assert!(is_flow_url("https://flow.google.com/"));
        assert!(is_flow_url("https://flow.google.com/project/6f2c?x=2#y"));
        assert!(is_flow_url("https://user@flow.google.com:443/"));
        assert!(!is_flow_url("https://evil.com/?next=flow.google.com"));
        assert!(!is_flow_url("https://flow.google.com.evil.com/"));
        assert!(!is_flow_url("https://aistudio.google.com/"));
        assert!(!is_flow_url("about:blank"));
        assert!(!is_flow_url(""));
    }
}

/// Print the tabs the extension can see, and mark the one commands would drive.
///
/// Worth having as a command: when two Flow tabs exist, every run silently adopts the first one,
/// and the symptom is a page that answers probes but never matches what you are looking at.
pub fn tabs() -> Result<()> {
    ensure_bridge()?;
    let engine = engine();
    let list = engine
        .execute("safari_list_tabs", &json!({}))
        .map_err(|error| anyhow::anyhow!("could not list Safari tabs: {error}"))?;
    let chosen = flow_tab_index(&list);
    for tab in list.as_array().cloned().unwrap_or_default() {
        let index = tab
            .get("index")
            .and_then(Value::as_i64)
            .or_else(|| tab.get("window_id").and_then(Value::as_str)?.parse().ok());
        let url = tab
            .get("url")
            .or_else(|| tab.get("safeUrl"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mark = if index == chosen { "  <- flowy uses this" } else { "" };
        println!("[{}] {url}{mark}", index.unwrap_or_default());
    }
    println!("{} tab(s), receipt: {}", list.as_array().map_or(0, Vec::len),
        if engine.current_receipt().is_some() { "owned" } else { "none" });
    if let Some(tab) = list
        .as_array()
        .and_then(|tabs| tabs.iter().find(|tab| {
            tab.get("index").and_then(Value::as_i64) == chosen
                || tab.get("window_id").and_then(Value::as_str) == chosen.map(|i| i.to_string()).as_deref()
        }))
    {
        println!("raw: {}", serde_json::to_string(tab)?);
    }
    Ok(())
}
