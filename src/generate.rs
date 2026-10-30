//! Turning a prompt into images.
//!
//! Flow's own "Start generation" button only acts on a *trusted* event. The check is in the
//! page, in `flow-base-prompt-box.submit`:
//!
//! ```text
//! submit(event) {
//!   ...
//!   if (!flagA || !flagB || (event != null && event.isTrusted)) { /* really generate */ }
//! }
//! ```
//!
//! A synthetic click arrives with `isTrusted: false`, and WebKit does not let a page redefine
//! it — not on `Event.prototype`, and not on the event instance (the own accessor is
//! configurable, but the engine still reports the real value). So clicking the button from a
//! page script is a dead end, and so is dispatching Enter into the editor.
//!
//! The way through is the binding the framework installs for the button's `generate` output.
//! That listener calls `submit(event)` with whatever it is handed, so handing it a plain
//! object whose `isTrusted` is `true` passes the check. The listener is only reachable while
//! the component is being created, so it is captured by hooking `addEventListener` and then
//! re-entering the project — an in-app route change, which keeps the hook alive because the
//! document is never replaced.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use crate::{
    net::{self, Reply},
    page,
    ui::{self, eval_patched},
};

const POLL: Duration = Duration::from_millis(250);

/// How long the set of new images has to stop growing before the batch counts as complete.
/// Flow renders the frames of one request a moment apart, so this is a debounce, not a wait.
const SETTLE: Duration = Duration::from_secs(2);

/// How long the generation RPC gets to answer before the DOM takes over. A request takes ~20s
/// end to end; a reply that has not arrived by now is not coming, and waiting out the full
/// timeout would be worse than reading the cards.
const RPC_WAIT: Duration = Duration::from_secs(60);

/// If the recorder has not even seen the request by now, it is not watching this page — the
/// app probably booted before it was installed. Fall back early rather than wait out [`RPC_WAIT`].
const RPC_PROBE: Duration = Duration::from_secs(4);

/// How long a fired trigger gets to show that it did something. A live submit clears the prompt
/// box and sends the request; a stale binding does neither.
const TRIGGER_WAIT: Duration = Duration::from_secs(6);

/// Hook `addEventListener` and keep the listener bound to the generate button's output.
/// Idempotent: the second call only reports what the first one captured.
const CAPTURE: &str = r#"(()=>{
  if(!window.__flowyHook){
    window.__flowyHook=1;
    const add=EventTarget.prototype.addEventListener;
    EventTarget.prototype.addEventListener=function(type,listener,options){
      try{
        if(this&&this.nodeType===1&&this.tagName==='FLOW-GENERATE-ICON-BUTTON'){
          if(type==='generate')window.__flowyGenerate=listener;
        }
      }catch(error){}
      return add.apply(this,arguments);
    };
  }
  return {hooked:true,generate:typeof window.__flowyGenerate==='function'};
})()"#;

/// What the page currently offers: the prompt, the button, and the generated images.
const STATE: &str = r#"(()=>{
  if(window.__flowyPump)window.__flowyPump(8);
  const editor=document.querySelector('flow-prompt-box .ProseMirror')||document.querySelector('.ProseMirror');
  const button=document.querySelector('button[aria-label="Start generation"]');
  const images=[...document.querySelectorAll('img')].map(img=>img.src)
    .filter(src=>src.includes('flow-content.google/image/'));
  const text=document.body?document.body.innerText:'';
  const failures=(text.match(/failed to generate|violate our policies/g)||[]).length;
  return {path:location.pathname,
    hooked:window.__flowyHook===1,
    generate:typeof window.__flowyGenerate==='function',
    prompt:editor?editor.innerText.trim():null,
    button:button?{disabled:!!button.disabled}:null,
    images:images,failures:failures,
    failure:text.includes('violate our policies')?'This generation might violate our policies'
      :text.includes('failed to generate')?'Sorry, this image failed to generate':null};})()"#;

/// Re-run the last `count` failed cards through the card's own Retry button. That path does
/// not look at the event's trust flag, so an ordinary click is enough.
fn retry_script(count: usize) -> String {
    format!(
        r#"(()=>{{
  const buttons=[...document.querySelectorAll('button,[role="button"]')]
    .filter(el=>(el.innerText||'').trim()==='Retry');
  let clicked=0;
  for(let index=buttons.length-1;index>=0&&clicked<{count};index--,clicked++)buttons[index].click();
  return {{clicked:clicked}};}})()"#
    )
}

/// Hand the captured listener an event that claims to be trusted. Nothing else is faked:
/// `submit` reads only `isTrusted` off it.
const FIRE: &str = r#"(()=>{
  if(typeof window.__flowyGenerate!=='function')return {fired:false,reason:'no generate listener captured'};
  window.__flowyGenerate({isTrusted:true,type:'generate',
    target:document.querySelector('flow-generate-icon-button'),
    preventDefault(){},stopPropagation(){}});
  return {fired:true};
})()"#;

#[derive(Debug, Clone, Deserialize, Default)]
struct State {
    #[serde(default)]
    path: String,
    #[serde(default)]
    hooked: bool,
    #[serde(default)]
    generate: bool,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    button: Option<Button>,
    #[serde(default)]
    images: Vec<String>,
    /// How many failure notices the page is showing. Flow keeps them on the cards, so the
    /// only meaningful reading is the change since the request was sent.
    #[serde(default)]
    failures: usize,
    #[serde(default)]
    failure: Option<String>,
}

/// Where the images were read from. A run says which path answered, so the fallback is visible
/// rather than assumed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Rpc,
    Dom,
}

/// How a request ended. Flow answers in one of three ways: images, a failure notice on the
/// cards, or nothing at all.
enum Outcome {
    Images(Vec<String>, Source),
    Failed(String),
    Timeout,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct Button {
    #[serde(default)]
    disabled: bool,
}

/// Generate images for `prompt` in the project the tab is on, and print the image URLs.
///
/// The trigger is the UI's own path — it needs a trusted event and a scored reCAPTCHA token, and
/// neither can be synthesised. The *results* are read from the generation RPC's reply when the
/// recorder is in place, because that stream closes exactly when the request ends; the DOM is the
/// fallback for when it is not.
///
/// Flow's image service fails a request often enough to matter — "Sorry, this image failed to
/// generate", or the policy notice — without anything being wrong with the request. Both are
/// retryable through the card's own Retry button, so a run retries instead of reporting a
/// failure the first time it sees one.
pub fn generate(prompt: &str, timeout: Duration, retries: usize, rpc_read: bool) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    let _serialized = page::serialize_requests()?;
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;

    // Installing the recorder means navigating, so it is best-effort: the DOM path below works
    // without it, just less precisely.
    let rpc = rpc_read && match net::ensure_recorder(engine) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("flowy: reading results from the DOM ({error:#})");
            false
        }
    };
    ui::wait_for_ready(engine, Duration::from_secs(45))?;

    capture_listener(engine)?;

    let before = state(engine)?;
    if before.button.is_none() {
        bail!(
            "no prompt box on {} — Flow has no project open there. Run `flowy open` to pick one; \
             if the project you expect is missing, it belongs to a different account than the \
             Safari profile flowy drives (`flowy profile`)",
            before.path
        );
    }

    arm(engine, prompt)?;

    let mut baseline = before;
    let mut last_failure = String::new();
    // The recorder's sequence number *before* the request goes out. Everything after it belongs
    // to this attempt.
    let mut mark = if rpc { net::mark(engine).unwrap_or(0) } else { 0 };
    for attempt in 0..=retries {
        if attempt == 0 {
            let (first_mark, took) = trigger(engine, prompt, rpc)?;
            mark = first_mark.unwrap_or(mark);
            if !took {
                // The captured binding belongs to a component that has since been destroyed, so
                // calling it does nothing at all — and the run would then wait out the whole
                // timeout for images that were never requested. Rebuild the box and try again.
                eprintln!("flowy: the trigger did not submit; rebuilding the prompt box");
                eval_patched(engine, FORGET)?;
                capture_listener(engine)?;
                arm(engine, prompt)?;
                baseline = state(engine)?;
                let (rebuilt_mark, took) = trigger(engine, prompt, rpc)?;
                mark = rebuilt_mark.unwrap_or(mark);
                if !took {
                    bail!(
                        "the generate button accepted nothing, even after the prompt box was rebuilt"
                    );
                }
            }
            println!("Generating: {prompt}");
        } else {
            let pending = state(engine)?.failures.saturating_sub(baseline.failures);
            if rpc {
                mark = net::mark(engine).unwrap_or(mark);
            }
            let retried = eval_patched(engine, &retry_script(pending.max(1)))?;
            let clicked = retried.get("clicked").and_then(Value::as_u64).unwrap_or(0);
            if clicked == 0 {
                bail!("generation failed ({last_failure}) and the card has no Retry button");
            }
            println!("Retry {attempt}/{retries} after: {last_failure}");
            // The retried card reports progress again, so the baseline moves with it.
            baseline = state(engine)?;
        }

        match wait_for_outcome(engine, &baseline, timeout, rpc.then_some(mark)) {
            Outcome::Images(images, source) => {
                for (index, url) in images.iter().enumerate() {
                    println!("{}. {url}", index + 1);
                }
                let via = match source {
                    Source::Rpc => "the generation RPC",
                    Source::Dom => "the DOM",
                };
                eprintln!("flowy: {} image(s) via {via}", images.len());
                return Ok(());
            }
            Outcome::Failed(message) => {
                last_failure = message;
                thread::sleep(Duration::from_secs(2));
            }
            Outcome::Timeout => bail!(
                "no image appeared within {}s{}",
                timeout.as_secs(),
                if last_failure.is_empty() {
                    String::new()
                } else {
                    format!(" (last failure: {last_failure})")
                }
            ),
        }
    }
    bail!("generation failed after {retries} retries: {last_failure}")
}

/// Drop the captured binding, so [`capture_listener`] rebuilds the prompt box instead of
/// reporting the dead one it still holds.
const FORGET: &str = "(()=>{window.__flowyGenerate=null;return true;})()";

/// Fire the captured binding and report whether the page actually submitted.
///
/// Returns the recorder mark taken *before* the click, so the caller can look for that request's
/// reply, and whether the submit took.
fn trigger(
    engine: &safari_driver::SafariEngine,
    prompt: &str,
    rpc: bool,
) -> Result<(Option<u64>, bool)> {
    let mark = rpc.then(|| net::mark(engine).unwrap_or(0));
    let fired = eval_patched(engine, FIRE)?;
    if fired.get("fired").and_then(Value::as_bool) != Some(true) {
        bail!(
            "could not trigger generation: {}",
            fired
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown reason")
        );
    }
    Ok((mark, trigger_took(engine, prompt, mark)))
}

/// Did the submit go through? A live one either reaches the recorder or clears the prompt box,
/// and Flow disables the button while a request is in flight. A dead binding does none of these.
fn trigger_took(
    engine: &safari_driver::SafariEngine,
    prompt: &str,
    mark: Option<u64>,
) -> bool {
    let deadline = Instant::now() + TRIGGER_WAIT;
    loop {
        if let Some(mark) = mark
            && net::saw_request(engine, mark).unwrap_or(false)
        {
            return true;
        }
        if let Ok(state) = state(engine)
            && state.button.is_some()
        {
            let cleared = state.prompt.as_deref().map(str::trim) != Some(prompt.trim());
            let busy = state.button.as_ref().is_some_and(|button| button.disabled);
            if cleared || busy {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
}

/// Put `prompt` in the box and wait until the button accepts it.
fn arm(engine: &safari_driver::SafariEngine, prompt: &str) -> Result<()> {
    let typed = type_prompt(engine, prompt)?;
    if typed.prompt.as_deref().map(str::trim) != Some(prompt.trim()) {
        bail!(
            "the prompt box did not take the text (it holds {:?})",
            typed.prompt.unwrap_or_default()
        );
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state = state(engine)?;
        if state.button.as_ref().is_some_and(|button| !button.disabled) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the generate button stayed disabled after the prompt was typed");
        }
        thread::sleep(POLL);
    }
}

/// Type into the prompt box without submitting.
pub fn prompt(text: &str) -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let state = type_prompt(engine, text)?;
    println!("{}", state.prompt.unwrap_or_default());
    Ok(())
}

/// Trigger generation for whatever the prompt box already holds.
pub fn fire() -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    capture_listener(engine)?;
    let fired = eval_patched(engine, FIRE)?;
    if fired.get("fired").and_then(Value::as_bool) != Some(true) {
        bail!("no generate listener captured for this page");
    }
    println!("Generation triggered.");
    Ok(())
}

/// List the generated images in the project the tab is on.
pub fn images() -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let state = state(engine)?;
    if state.images.is_empty() {
        println!("No generated images on {}.", state.path);
        return Ok(());
    }
    for (index, url) in state.images.iter().enumerate() {
        println!("{}. {url}", index + 1);
    }
    Ok(())
}

/// Download generated images. The content URLs are public: they carry their own signature,
/// so plain `curl` fetches them without the browser session.
pub fn download(index: Option<usize>, out: &Path) -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let state = state(engine)?;
    if state.images.is_empty() {
        bail!("no generated images on {}", state.path);
    }
    let selected: Vec<&String> = match index {
        Some(index) => {
            let image = state.images.get(index.saturating_sub(1)).ok_or_else(|| {
                anyhow::anyhow!(
                    "there is no image {index}; the project has {}",
                    state.images.len()
                )
            })?;
            vec![image]
        }
        None => state.images.iter().collect(),
    };
    std::fs::create_dir_all(out)
        .map_err(|error| anyhow::anyhow!("could not create {}: {error}", out.display()))?;
    for (offset, url) in selected.iter().enumerate() {
        let number = index.unwrap_or(offset + 1);
        let path = fetch(url, out, number)?;
        println!("{}", path.display());
    }
    Ok(())
}

/// Fetch one image and name it after the id in its URL, keeping the served extension.
fn fetch(url: &str, out: &Path, number: usize) -> Result<std::path::PathBuf> {
    let id = url
        .split("flow-content.google/image/")
        .nth(1)
        .and_then(|rest| rest.split(['?', '/']).next())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{number:02}"));
    let stem = out.join(format!("flowy-{number:02}-{id}"));
    let partial = stem.with_extension("part");
    let output = std::process::Command::new("curl")
        .args(["-sSL", "--fail", "-o"])
        .arg(&partial)
        .arg("-w")
        .arg("%{content_type}")
        .arg(url)
        .output()
        .map_err(|error| anyhow::anyhow!("could not run curl: {error}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&partial);
        bail!(
            "download failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let content_type = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let extension = match content_type.as_str() {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "jpg",
    };
    let final_path = stem.with_extension(extension);
    std::fs::rename(&partial, &final_path)
        .map_err(|error| anyhow::anyhow!("could not write {}: {error}", final_path.display()))?;
    Ok(final_path)
}

/// Make sure the generate binding is captured for the component that is on screen.
///
/// The listener only exists while the prompt box is being built, so a page that already booted
/// cannot be patched into shape. Reloading is not an option either: `safari_navigate` returns
/// once the document has loaded, by which point the app has already built the prompt box. What
/// does work is a route change the app performs itself — leaving the project and entering it
/// again rebuilds the component, and the hook survives because the document is never replaced.
fn capture_listener(engine: &safari_driver::SafariEngine) -> Result<()> {
    eval_patched(engine, CAPTURE)?;
    if state(engine)?.generate {
        return Ok(());
    }

    let current = state(engine)?;
    let target = match project_id(&current.path) {
        Some(id) => format!("/project/{id}"),
        // On the home screen there is no prompt box to drive; open the newest project, which
        // is what a person would do next anyway.
        None => first_project_link(engine)?,
    };

    if current.path != "/" {
        let home = eval_patched(engine, GO_HOME)?;
        if home.get("clicked").and_then(Value::as_bool) != Some(true) {
            bail!("could not find the Flow home button to rebuild the prompt box");
        }
        wait_for_path(engine, "/", Duration::from_secs(20))?;
    }

    // The home route paints its project grid after the route change resolves, so the link is
    // not there on the first look.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let open = eval_patched(engine, &open_project_script(&target))?;
        if open.get("clicked").and_then(Value::as_bool) == Some(true) {
            break;
        }
        if Instant::now() >= deadline {
            bail!("could not re-open {target} from the Flow home screen");
        }
        thread::sleep(POLL);
    }
    wait_for_path(engine, &target, Duration::from_secs(20))?;
    wait_for_generate(engine, Duration::from_secs(30))
}

/// Wait for the prompt box to be created and to hand over its generate binding.
fn wait_for_generate(engine: &safari_driver::SafariEngine, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        // Evaluations fail while the document is being replaced, which is expected here.
        if let Ok(state) = state(engine)
            && state.generate
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Telling these two apart matters: a missing hook means the injection lost the race
            // with the bundle, while a missing binding means the page never built a prompt box.
            if !state(engine).map(|state| state.hooked).unwrap_or(false) {
                bail!("the page loaded but the capture hook never installed");
            }
            bail!("the project loaded but never exposed its generate binding");
        }
        thread::sleep(POLL);
    }
}

/// Leave the project without reloading: the hook only survives an in-app route change.
const GO_HOME: &str = r#"(()=>{
  for(const element of document.querySelectorAll('a,button,[role="button"]')){
    if((element.getAttribute('aria-label')||'').trim()==='Home'){element.click();return {clicked:true};}
  }
  return {clicked:false};})()"#;

fn open_project_script(path: &str) -> String {
    format!(
        r#"(()=>{{
  const link=document.querySelector('a[href="{}"]');
  if(!link)return {{clicked:false}};
  link.click();
  return {{clicked:true}};}})()"#,
        path.replace('"', "")
    )
}

fn wait_for_path(engine: &safari_driver::SafariEngine, path: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let current = state(engine)?.path;
        if current == path {
            return Ok(());
        }
        // Flow answers a project id that does not belong to the signed-in account with a
        // redirect to /404, not with an error. The page then renders as an empty shell, which
        // looks like "the app never booted" unless the redirect is named.
        if is_missing_project(&current) {
            bail!(
                "{path} is not in the account flowy is driving: Flow redirected to /404. \
                 Check `flowy profile` — the project has to exist in that Safari profile's account"
            );
        }
        if Instant::now() >= deadline {
            bail!("the Flow tab never reached {path} (it is on {current})");
        }
        thread::sleep(POLL);
    }
}

/// Flow's "this project is not in your account" redirect.
fn is_missing_project(path: &str) -> bool {
    path == "/404" || path.starts_with("/404/")
}

fn project_id(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/project/")?;
    let id = rest.split('/').next().unwrap_or_default();
    (!id.is_empty()).then_some(id)
}

fn first_project_link(engine: &safari_driver::SafariEngine) -> Result<String> {
    let value = eval_patched(
        engine,
        r#"(()=>{
  const link=document.querySelector('a[href^="/project/"]');
  return link?link.getAttribute('href'):null;})()"#,
    )?;
    match value.as_str() {
        Some(href) if href.starts_with("/project/") => Ok(href.to_owned()),
        _ => bail!(
            "there is no project on the Flow home screen to generate in. Create one in the \
             account the Safari profile flowy drives (`flowy profile`)"
        ),
    }
}

/// Wait for the request to end.
///
/// With a recorder in place the generation RPC answers first: its stream closes only when the
/// request is over, so the reply needs no debounce and does not depend on the card being
/// rendered. The DOM is consulted when the recorder is absent, when it never saw the request,
/// or when the reply is late — and a DOM reading still wins over an RPC failure, because Flow
/// can answer a two-image request with one picture and one failure notice, and the picture is
/// worth having.
fn wait_for_outcome(
    engine: &safari_driver::SafariEngine,
    before: &State,
    timeout: Duration,
    rpc_mark: Option<u64>,
) -> Outcome {
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    // New means "not on the page when the request was sent". Comparing sets rather than counts
    // keeps this correct when the project list virtualises and drops older cards.
    let seen: HashSet<&str> = before.images.iter().map(String::as_str).collect();
    let mut last = 0usize;
    let mut settled: Option<(Instant, Vec<String>)> = None;
    let mut dom = rpc_mark.is_none();
    loop {
        // Checked first: an evaluation can fail for as long as the document is being replaced,
        // and that must not become an unbounded wait.
        if Instant::now() >= deadline {
            return match settled {
                Some((_, images)) => Outcome::Images(images, Source::Dom),
                None => Outcome::Timeout,
            };
        }

        if let Some(mark) = rpc_mark {
            match net::reply_since(engine, mark) {
                Ok(Some(Reply::Images(images))) => return Outcome::Images(images, Source::Rpc),
                Ok(Some(Reply::Failed(message))) => {
                    // A DOM reading outranks this: the reply describes the RPC, not the cards.
                    if settled.is_none() {
                        return Outcome::Failed(message);
                    }
                }
                _ => {}
            }
            if !dom {
                let waited = started.elapsed();
                let unwatched = waited >= RPC_PROBE
                    && !net::saw_request(engine, mark).unwrap_or(true);
                if unwatched || waited >= RPC_WAIT {
                    dom = true;
                }
            }
        }

        if dom {
            let Ok(state) = state(engine) else {
                thread::sleep(POLL);
                continue;
            };
            let new: Vec<String> = state
                .images
                .into_iter()
                .filter(|url| !seen.contains(url.as_str()))
                .collect();
            if new.len() > last {
                last = new.len();
                settled = Some((Instant::now(), new));
            } else if let Some((seen_at, images)) = &settled
                && seen_at.elapsed() >= SETTLE
            {
                return Outcome::Images(images.clone(), Source::Dom);
            }
            // A retried card clears its notice, so only a notice that outlives the baseline counts.
            if settled.is_none() && state.failures > before.failures {
                return Outcome::Failed(
                    state
                        .failure
                        .unwrap_or_else(|| "the request failed".to_owned()),
                );
            }
        }
        thread::sleep(POLL);
    }
}

fn state(engine: &safari_driver::SafariEngine) -> Result<State> {
    let value = eval_patched(engine, STATE)?;
    serde_json::from_value(value).map_err(|error| anyhow::anyhow!("could not read the page: {error}"))
}

/// Put `text` in the prompt box. `execCommand('insertText')` is the one writing path that
/// ProseMirror accepts from a script: the browser performs the edit itself, so the editor's
/// mutation observer and the form's model both see it.
fn type_prompt(engine: &safari_driver::SafariEngine, text: &str) -> Result<State> {
    let script = format!(
        r#"(()=>{{
  const text={text};
  const editor=document.querySelector('flow-prompt-box .ProseMirror')||document.querySelector('.ProseMirror');
  if(!editor)return {{typed:false}};
  editor.focus();
  const range=document.createRange();
  range.selectNodeContents(editor);
  range.collapse(false);
  const selection=getSelection();
  selection.removeAllRanges();
  selection.addRange(range);
  const typed=document.execCommand('insertText',false,text);
  return {{typed:typed,text:editor.innerText}};}})()"#,
        text = serde_json::to_string(text)?
    );
    eval_patched(engine, &script)?;
    state(engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_the_missing_project_redirect() {
        assert!(is_missing_project("/404"));
        assert!(is_missing_project("/404/"));
        assert!(!is_missing_project("/4040"));
        assert!(!is_missing_project("/project/6f2c"));
    }

    #[test]
    fn reads_the_project_id_from_a_project_path() {
        assert_eq!(
            project_id("/project/46b49b0b-fe2b-453a-aed3-3930bb88972c"),
            Some("46b49b0b-fe2b-453a-aed3-3930bb88972c")
        );
        assert_eq!(project_id("/project/6f2c/frames"), Some("6f2c"));
        assert_eq!(project_id("/"), None);
        assert_eq!(project_id("/project/"), None);
    }
}
