//! The direct path: Flow's own `batchexecute` RPC, called from the page.
//!
//! Driving the UI costs a render loop, a captured event listener, and ~20s of waiting on a
//! card that may fail. Flow's front end does something much simpler: it POSTs an `f.req`
//! payload to `/_/AiSandboxAngularFrontend/data/batchexecute` and reads the reply. Doing that
//! directly skips the UI entirely.
//!
//! The catch is that the front end keeps a reference to `fetch` taken at module load, so a
//! patch installed after the app boots never sees those requests. [`install`] therefore runs
//! right after a navigation, before the bundle has executed.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{thread, time::Duration};

use crate::page;

/// Record every request the app makes, including the body. Installed as early as possible.
const INSTALL: &str = r#"(()=>{
  if(window.__flowyNetInstalled)return {installed:true,url:location.href};
  window.__flowyNetInstalled=1;
  const log=window.__flowyNet=[];
  const push=(kind,url,body)=>{
    if(log.length>300)return;
    log.push({kind:kind,url:String(url||'').slice(0,400),body:String(body||'').slice(0,12000)});
  };
  const nativeFetch=window.fetch;
  window.fetch=function(input,init){
    try{
      const url=(typeof input==='string')?input:(input&&input.url)||'';
      if(init&&typeof init.body==='string')push('fetch',url,init.body);
      else if(input&&input.clone)input.clone().text().then(text=>push('fetch',url,text)).catch(error=>push('fetch-locked',url,String(error)));
      else push('fetch',url,'');
    }catch(error){}
    return nativeFetch.apply(this,arguments);
  };
  // The app builds `new Request(url, {body})` for the RPCs it streams, and a Request whose
  // body has been handed to fetch can no longer be cloned — so the body is read here, at
  // construction, where it is still a plain string.
  const NativeRequest=window.Request;
  if(NativeRequest){
    const Wrapped=function(input,init){
      try{
        if(init&&typeof init.body==='string')push('request',input,init.body);
        else if(init&&init.body)push('request',input,'<'+Object.prototype.toString.call(init.body)+'>');
      }catch(error){}
      return new NativeRequest(input,init);
    };
    Wrapped.prototype=NativeRequest.prototype;
    window.Request=Wrapped;
  }
  const open=XMLHttpRequest.prototype.open;
  const send=XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open=function(method,url){this.__flowyUrl=url;return open.apply(this,arguments);};
  XMLHttpRequest.prototype.send=function(body){
    try{push('xhr',this.__flowyUrl,typeof body==='string'?body:'');}catch(error){}
    return send.apply(this,arguments);
  };
  return {installed:true,url:location.href};
})()"#;

/// What the app itself would send: the tokens that live on the page.
const TOKENS: &str = r#"(()=>{
  const data=window.WIZ_global_data||{};
  return {at:data.SNlM0e||null,
    bl:data.cfb2h||null,
    fsid:data.FdrFJe||null,
    hsid:data.hsid||null,
    path:location.pathname,
    net:Array.isArray(window.__flowyNet)?window.__flowyNet.length:null};})()"#;

/// Install the recorder in the document that is loading now.
///
/// `safari_navigate` returns as soon as the navigation starts, so this races the app's bundle.
/// The install only counts once it has landed in the *new* document, which is why the reply is
/// checked against the stamp in the URL.
pub fn install() -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let path = current_path(engine)?;
    inject_at_document_start(&path, INSTALL)?;
    println!("Recorder installed on {path}");
    Ok(())
}

/// Load `path` and run `script` in the new document before the app's bundle executes.
///
/// The bundle keeps references to `fetch`, `Request`, and `addEventListener` taken at module
/// load, so anything installed after it boots is invisible to the app. Navigating and injecting
/// immediately — the bundle is megabytes and takes seconds to execute — wins that race.
pub fn inject_at_document_start(path: &str, script: &str) -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    reload_with(engine, path, script)
}

fn reload_with(
    engine: &safari_driver::SafariEngine,
    path: &str,
    script: &str,
) -> Result<()> {
    let origin = page::eval(engine, "location.origin")?
        .as_str()
        .unwrap_or("https://flow.google.com")
        .to_owned();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let url = format!("{origin}{path}?flowy={stamp}");

    engine
        .execute("safari_navigate", &json!({"url": url}))
        .map_err(|error| anyhow::anyhow!("could not navigate to {url}: {error}"))?;

    // The script is wrapped so its own reply can be checked against the stamp: an evaluation
    // that answers from the *old* document looks successful but installed nothing.
    let wrapped = format!(
        "(()=>{{const result=({script});return {{url:location.href,result:result}};}})()"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(value) = page::eval(engine, &wrapped) {
            let href = value.get("url").and_then(Value::as_str).unwrap_or_default();
            if href.contains(&format!("flowy={stamp}")) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!("the script never landed in the new document; the app probably booted first");
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn current_path(engine: &safari_driver::SafariEngine) -> Result<String> {
    Ok(page::eval(engine, "location.pathname")?
        .as_str()
        .unwrap_or("/")
        .to_owned())
}

/// Print what the recorder caught, and the page tokens the direct call would need.
pub fn dump(filter: Option<&str>) -> Result<()> {
    page::ensure_bridge()?;
    let engine = page::engine();
    page::ensure_tab(engine)?;
    let tokens = page::eval(engine, TOKENS)?;
    println!("{}", serde_json::to_string_pretty(&tokens)?);

    let log = page::eval(engine, "window.__flowyNet||[]")?;
    let entries = log.as_array().cloned().unwrap_or_default();
    for entry in &entries {
        let url = entry.get("url").and_then(Value::as_str).unwrap_or_default();
        if let Some(filter) = filter
            && !url.contains(filter)
        {
            continue;
        }
        let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("?");
        let body = entry.get("body").and_then(Value::as_str).unwrap_or_default();
        println!("--- {kind} {url}");
        if !body.is_empty() {
            println!("{body}");
        }
    }
    println!("{} request(s) recorded", entries.len());
    Ok(())
}
