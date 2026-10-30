//! Flow page automation.
//!
//! Phase one: a live tab, the background-frame patch, and a way to read what the page is
//! showing. The generation flow is built on top of what [`status`] reports, so the selectors
//! come from the page rather than from a guess.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::{
    thread,
    time::{Duration, Instant},
};

use crate::page::{self, FLOW_URL};

const POLL: Duration = Duration::from_millis(400);

/// Flow is an Angular app, and Safari stops delivering `requestAnimationFrame` to a hidden
/// document, so a background tab never repaints. Frames are held while the document is
/// hidden and released on demand by the probe: nothing runs on its own, so an idle tab
/// costs nothing, and the cost per poll is bounded by the pump budget.
const SHIM: &str = r#"(()=>{
  if(window.__flowyShim===1)return {patched:true,hidden:document.hidden,frames:window.__flowyFrames||0};
  window.__flowyShim=1;
  window.__flowyFrames=0;
  const nativeRaf=window.requestAnimationFrame.bind(window);
  const nativeCancel=window.cancelAnimationFrame.bind(window);
  const BASE=1e9;
  const waiting=new Map();
  let queue=[],nextId=1;
  window.requestAnimationFrame=callback=>{
    if(typeof callback!=='function'||!document.hidden)return nativeRaf(callback);
    const id=BASE+nextId++;
    waiting.set(id,callback);
    queue.push(id);
    return id;
  };
  window.cancelAnimationFrame=id=>{
    if(waiting.delete(id))return;
    if(id<BASE)nativeCancel(id);
  };
  document.addEventListener('visibilitychange',()=>{
    if(document.hidden||!queue.length)return;
    const parked=queue;queue=[];
    for(const id of parked){
      const callback=waiting.get(id);
      if(!callback)continue;
      waiting.delete(id);
      nativeRaf(callback);
    }
  });
  window.__flowyPump=budget=>{
    const started=performance.now();
    const deadline=started+(budget||8);
    let ran=0;
    while(queue.length&&performance.now()<deadline){
      const id=queue.shift();
      const callback=waiting.get(id);
      if(!callback)continue;
      waiting.delete(id);
      ran++;
      try{callback(started);}catch(error){setTimeout(()=>{throw error;},0);}
    }
    window.__flowyFrames+=ran;
    return ran;
  };
  return {patched:true,hidden:document.hidden,frames:0};
})()"#;

/// A snapshot of the page: what is on screen, what can be clicked, what can be typed into.
/// Wide on purpose — during a reverse-engineering pass the useful thing is to see the page,
/// not to assert a shape it may not have.
const PROBE: &str = r#"(()=>{
  if(window.__flowyPump)window.__flowyPump(8);
  const shown=el=>{const r=el.getBoundingClientRect();return r.width>0&&r.height>0;};
  const label=el=>(el.getAttribute('aria-label')||el.innerText||el.getAttribute('placeholder')||'')
    .replace(/\s+/g,' ').trim().slice(0,48);
  const buttons=[...document.querySelectorAll('button,[role="button"]')].filter(shown);
  const inputs=[...document.querySelectorAll('[contenteditable="true"],textarea,input')]
    .filter(shown).filter(el=>el.getAttribute('type')!=='hidden');
  return {url:location.href,title:document.title,ready:document.readyState,
    hidden:document.hidden,
    buttons:buttons.length,labels:buttons.map(label).filter(Boolean).slice(0,30),
    inputs:inputs.map(el=>({tag:el.tagName.toLowerCase(),label:label(el)})).slice(0,10),
    images:[...document.images].filter(shown).length,
    videos:document.querySelectorAll('video').length,
    text:(document.body?document.body.innerText:'').replace(/\s+/g,' ').trim().slice(0,400),
    signedOut:location.hostname==='accounts.google.com'
      ||!!document.querySelector('a[href*="ServiceLogin"],a[href*="accounts.google.com/signin"]')};})()"#;

/// Whether the patch is live in the current document. Read before probing, because the
/// probe is what installs it.
const PATCH_STATE: &str = "({patched:window.__flowyShim===1,\
  frames:window.__flowyFrames||0,hidden:document.hidden})";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Probe {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    ready: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    buttons: usize,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    inputs: Vec<Input>,
    #[serde(default)]
    images: usize,
    #[serde(default)]
    videos: usize,
    #[serde(default)]
    text: String,
    #[serde(default, rename = "signedOut")]
    signed_out: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct Input {
    #[serde(default)]
    tag: String,
    #[serde(default)]
    label: String,
}

/// Open Flow in the owned tab.
pub fn open_flow() -> Result<()> {
    page::ensure_bridge()?;
    page::ensure_tab(page::engine())?;
    println!("Flow is open in Safari.");
    Ok(())
}

/// Reload the owned tab. The cache-busting stamp matters: the driver's reload does not
/// bypass the cache, and a page loaded before signing in keeps showing the signed-out shell.
pub fn reload_flow() -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    page::navigate(engine, &format!("{FLOW_URL}?reload={stamp}"))?;
    page::wait_for_page(engine)?;
    patch_page(engine)?;
    println!("Flow reloaded.");
    Ok(())
}

pub fn eval_script(script: &str) -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let value = page::eval(engine, script)?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Long page scripts are unreadable once they go through shell quoting, so they live in
/// files under `tools/` and are read here instead.
pub fn eval_file(path: &str) -> Result<()> {
    let script = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("could not read {path}: {error}"))?;
    eval_script(script.trim())
}

/// What the automation currently sees, for when a run goes wrong.
pub fn status() -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let patch = page::eval(engine, PATCH_STATE).ok();
    let probe = probe(engine)?;

    println!("url:        {}", probe.url);
    println!("title:      {}", probe.title);
    println!("document:   {}", probe.ready);
    println!(
        "tab:        {}",
        if probe.hidden { "hidden" } else { "visible" }
    );
    let frames = patch
        .as_ref()
        .and_then(|value| value.get("frames"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let patched = patch
        .as_ref()
        .and_then(|value| value.get("patched"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!(
        "render:     {}",
        if patched {
            format!("patched, {frames} frames pumped while hidden")
        } else {
            "not patched yet — the next command installs it".to_owned()
        }
    );
    println!("buttons:    {}", probe.buttons);
    println!("inputs:     {}", probe.inputs.len());
    for input in &probe.inputs {
        println!("            {} · {}", input.tag, input.label);
    }
    println!("images:     {}", probe.images);
    println!("videos:     {}", probe.videos);
    if !probe.labels.is_empty() {
        println!("clickable:");
        for label in &probe.labels {
            println!("            {label}");
        }
    }
    println!("text:       {}", preview(&probe.text, 240));
    if probe.signed_out {
        println!("signed in:  NO — sign in to Google in this Safari profile");
    }
    Ok(())
}

/// Bring the tab to the front, for the case the background patch cannot wake up.
pub fn focus_tab() -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    page::focus_tab(engine)?;
    page::wait_until_visible(engine, Duration::from_secs(10))?;
    println!("Flow tab is in front and rendering.");
    Ok(())
}

/// Install the background-tab patch. Navigation replaces the document, so this runs after
/// every load rather than once per process; every later page script carries it too.
fn patch_page(engine: &safari_driver::SafariEngine) -> Result<()> {
    page::eval(engine, SHIM).map(|_| ())
}

fn probe(engine: &safari_driver::SafariEngine) -> Result<Probe> {
    let value = page::eval(engine, &patched(PROBE))?;
    serde_json::from_value(value)
        .map_err(|error| anyhow::anyhow!("could not read the page: {error}"))
}

/// Run a page script with the background-render patch in front of it.
pub fn eval_patched(engine: &safari_driver::SafariEngine, script: &str) -> Result<Value> {
    page::eval(engine, &patched(script))
}

/// Prefix a page script with the patch. `evaluate_json` stringifies a single expression, so
/// the parts are joined with the comma operator; statements would be a syntax error there.
fn patched(script: &str) -> String {
    format!("({SHIM},({script}))")
}

/// Wait until the app has parsed and is not sitting on a sign-in page.
pub fn wait_for_ready(engine: &safari_driver::SafariEngine, timeout: Duration) -> Result<Probe> {
    let deadline = Instant::now() + timeout;
    let mut last = Probe::default();
    loop {
        if let Ok(probe) = probe(engine) {
            if probe.signed_out {
                bail!(
                    "the selected Safari profile is signed out of Google ({}). Sign in at accounts.google.com in that profile, then rerun",
                    probe.url
                );
            }
            if probe.ready != "loading" {
                return Ok(probe);
            }
            last = probe;
        }
        if Instant::now() >= deadline {
            bail!(
                "Flow never finished loading (url: {}, title: {}, document: {}). Run `flowy status`",
                last.url,
                last.title,
                last.ready
            );
        }
        thread::sleep(POLL);
    }
}

fn preview(text: &str, limit: usize) -> String {
    let short: String = text.chars().take(limit).collect();
    if short.is_empty() {
        "(empty)".to_owned()
    } else {
        short
    }
}
