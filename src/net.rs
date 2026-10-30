//! Reading Flow's own `batchexecute` replies.
//!
//! The generation RPC (`ogiZ0b`) answers with the finished image URLs, their dimensions, and the
//! prompt that produced them — on the `rt=c` streaming protocol, in length-prefixed chunks. The
//! stream closes only when the request is over, so its reply is a *definitive* completion signal,
//! unlike the DOM, where a card can be mid-paint.
//!
//! This module is the read half of the hybrid: [`crate::generate`] still triggers through the UI
//! (that path needs a trusted event and a scored reCAPTCHA token, neither of which can be
//! synthesised), and takes the *results* from here, falling back to the DOM when the recorder is
//! not in place.
//!
//! The catch is that the front end keeps a reference to `fetch` taken at module load, so a patch
//! installed after the app boots never sees those requests. [`install`] therefore runs right after
//! a navigation, before the bundle has executed.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{thread, time::Duration};

use crate::page;

/// The generation RPC. Its reply is the authoritative record of what was produced.
const RPC: &str = "ogiZ0b";

/// Where finished images live. The URL carries its own signature, so it needs no session.
const IMAGE_HOST: &str = "https://flow-content.google/image/";

/// Record every request the app makes, including the body, and the reply to each RPC.
/// Installed as early as possible.
const INSTALL: &str = r#"(()=>{
  if(window.__flowyNetVersion>=2)return {installed:true,url:location.href};
  // Versioned, because "something is installed" is not the same as "it has what we need": a
  // recorder left in the page by an older build answers every probe and then never matches.
  window.__flowyNetVersion=2;
  window.__flowyNetInstalled=1;
  const log=window.__flowyNet=[];
  // Entries carry a sequence number so a caller can ask for "everything after N" without
  // caring that the window slides. Keeping a fixed window instead of refusing writes matters:
  // the recorder is installed at document start and may outlive hundreds of requests.
  let seq=0;
  const push=(kind,url,body)=>{
    if(log.length>300)log.shift();
    log.push({seq:++seq,kind:kind,url:String(url||'').slice(0,400),body:String(body||'').slice(0,16000)});
    window.__flowyNetSeq=seq;
  };
  const nativeFetch=window.fetch;
  window.fetch=function(input,init){
    const url=(typeof input==='string')?input:(input&&input.url)||'';
    try{
      if(init&&typeof init.body==='string')push('fetch',url,init.body);
      else if(init&&init.body&&(init.body instanceof Uint8Array||init.body instanceof ArrayBuffer||ArrayBuffer.isView(init.body))){
        const body=init.body;
        const bytes=body instanceof ArrayBuffer?new Uint8Array(body)
          :new Uint8Array(body.buffer,body.byteOffset,body.byteLength);
        push('fetch-bin',url,new TextDecoder('utf-8').decode(bytes).slice(0,4000));
      }
      else if(input&&input.clone)input.clone().text().then(text=>push('fetch',url,text)).catch(error=>push('fetch-locked',url,String(error)));
      else push('fetch',url,'');
    }catch(error){}
    const reply=nativeFetch.apply(this,arguments);
    // The generation RPC answers on the streaming protocol, and its frames are the only
    // place the finished image URLs appear before the DOM has painted them. Reading the
    // clone costs the app nothing: it consumes the original.
    try{
      if(url.includes('batchexecute')&&reply&&reply.then)reply.then(response=>{
        try{response.clone().text().then(text=>push('resp',url,text)).catch(()=>{});}catch(error){}
      }).catch(()=>{});
    }catch(error){}
    return reply;
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

/// What a recorded generation reply says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Images(Vec<String>),
    Failed(String),
}

/// Make sure the recorder is in place, installing it if it is not.
///
/// Installing means navigating, because the app keeps its own `fetch` reference from module load
/// and a patch added to an already-booted page is invisible to it. A caller that gets an error
/// here should read the DOM instead — the trigger does not depend on this.
pub fn ensure_recorder(engine: &safari_driver::SafariEngine) -> Result<()> {
    let installed = page::eval(engine, "(window.__flowyNetVersion||0)>=2")?
        .as_bool()
        .unwrap_or(false);
    if installed {
        return Ok(());
    }
    let path = current_path(engine)?;
    inject_at_document_start(&path, INSTALL)
}

/// The recorder's sequence number right now. Take this *before* sending the request, then hand it
/// to [`reply_since`].
pub fn mark(engine: &safari_driver::SafariEngine) -> Result<u64> {
    Ok(page::eval(engine, "window.__flowyNetSeq||0")?
        .as_u64()
        .unwrap_or(0))
}

/// Whether the recorder has seen the generation request itself since `mark`.
///
/// This separates "the request is still running" from "the recorder is not watching this page":
/// a reply can only be expected from a request the recorder saw go out.
pub fn saw_request(engine: &safari_driver::SafariEngine, mark: u64) -> Result<bool> {
    let script = format!(
        r#"(()=>{{
  const log=window.__flowyNet||[];
  return log.some(entry=>entry.seq>{mark}&&entry.kind!=='resp'
    &&String(entry.url||'').indexOf('rpcids={RPC}')>=0);}})()"#
    );
    Ok(page::eval(engine, &script)?.as_bool().unwrap_or(false))
}

/// The first generation reply recorded after `mark`, if one has arrived.
///
/// `None` means "nothing yet" — the request is still in flight. The stream only closes when the
/// request is over, so a body here is a complete answer, not a partial one.
pub fn reply_since(engine: &safari_driver::SafariEngine, mark: u64) -> Result<Option<Reply>> {
    let script = format!(
        r#"(()=>{{
  const log=window.__flowyNet||[];
  const hits=log.filter(entry=>entry.kind==='resp'&&entry.seq>{mark}
    &&String(entry.url||'').indexOf('rpcids={RPC}')>=0);
  return {{hits:hits.length,body:hits.length?hits[hits.length-1].body:null}};}})()"#
    );
    let value = page::eval(engine, &script)?;
    let Some(body) = value.get("body").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(parse_stream(body))
}

/// Read a `batchexecute` stream: an XSSI prefix, then alternating length lines and JSON rows.
fn parse_stream(body: &str) -> Option<Reply> {
    let mut payload: Option<String> = None;
    let mut saw_row = false;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(")]}'") {
            continue;
        }
        // The length prefix of the next chunk.
        if line.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(rows) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(rows) = rows.as_array() else {
            continue;
        };
        for row in rows {
            let Some(row) = row.as_array() else {
                continue;
            };
            if row.first().and_then(Value::as_str) != Some("wrb.fr")
                || row.get(1).and_then(Value::as_str) != Some(RPC)
            {
                continue;
            }
            saw_row = true;
            // The result is itself a JSON document, carried as a string.
            if let Some(text) = row.get(2).and_then(Value::as_str) {
                payload = Some(text.to_owned());
            }
        }
    }

    if let Some(text) = payload {
        match serde_json::from_str::<Value>(&text) {
            Ok(parsed) => {
                let urls = images_in(&parsed);
                if !urls.is_empty() {
                    return Some(Reply::Images(urls));
                }
            }
            // A body we cannot read is not a verdict: say nothing and let the caller's DOM
            // fallback make the call.
            Err(_) => return None,
        }
    }
    if let Some(code) = error_code(body) {
        return Some(Reply::Failed(format!("the RPC refused the request ({code})")));
    }
    saw_row.then(|| Reply::Failed("the RPC returned no image".to_owned()))
}

/// The RPC reports refusals as an `ErrorInfo` row, which survives as a bare token in the stream.
fn error_code(body: &str) -> Option<String> {
    let rest = body.split("PUBLIC_ERROR_").nth(1)?;
    let code: String = rest
        .chars()
        .take_while(|character| character.is_ascii_uppercase() || character.is_ascii_digit() || *character == '_')
        .collect();
    (!code.is_empty()).then(|| format!("PUBLIC_ERROR_{code}"))
}

fn images_in(value: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    collect_urls(value, &mut urls);
    urls
}

fn collect_urls(value: &Value, urls: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if text.starts_with(IMAGE_HOST) && !urls.iter().any(|seen| same_image(seen, text)) {
                urls.push(text.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_urls(item, urls);
            }
        }
        Value::Object(fields) => {
            for item in fields.values() {
                collect_urls(item, urls);
            }
        }
        _ => {}
    }
}

/// The reply carries each URL twice — once on the card, once in the metadata block — with
/// different signatures, so identity is the image id rather than the whole string.
fn same_image(left: &str, right: &str) -> bool {
    image_id(left) == image_id(right)
}

fn image_id(url: &str) -> &str {
    url.split(IMAGE_HOST)
        .nth(1)
        .unwrap_or("")
        .split(['?', '/'])
        .next()
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real reply: two images, each URL present twice.
    const STREAM: &str = r#")]}'

2207
[["wrb.fr","ogiZ0b","[[[\"81af23d2-ffe7-4890-a414-2066ed498d91\",null,\"6a2c39b8\",null,null,null,[[null,440549435,null,null,null,null,1,\"a copper kettle\",36,null,null,\"6a2c39b8\",null,\"https://flow-content.google/image/81af23d2-ffe7-4890-a414-2066ed498d91?Expires=1793388907\",3,null,null,\"81af23d2-ffe7-4890-a414-2066ed498d91\"],null,[1376,768],null,null,[null,\"https://flow-content.google/image/81af23d2-ffe7-4890-a414-2066ed498d91?Expires=1793388907\"]]],[\"246e26dc-a0bc-4aeb-a355-659afe8238b0\",null,\"56bc5c3c\",null,null,null,[[null,1433367357,null,null,null,null,1,\"a copper kettle\",36,null,null,\"56bc5c3c\",null,\"https://flow-content.google/image/246e26dc-a0bc-4aeb-a355-659afe8238b0?Expires=1793388907\",3,null,null,\"246e26dc-a0bc-4aeb-a355-659afe8238b0\"],null,[1376,768],null,null,[null,\"https://flow-content.google/image/246e26dc-a0bc-4aeb-a355-659afe8238b0?Expires=1793388907\"]]]]]",null,null,null,"generic"]]
61
[["di",25238],["af.httprm",25237,"3367880335285021310",54]]
26
[["e",4,null,null,2308]]
"#;

    #[test]
    fn reads_the_image_urls_out_of_a_stream() {
        let Some(Reply::Images(urls)) = parse_stream(STREAM) else {
            panic!("expected images");
        };
        assert_eq!(urls.len(), 2, "each image appears twice and must be deduped");
        assert!(urls[0].contains("81af23d2-ffe7-4890-a414-2066ed498d91"));
        assert!(urls[1].contains("246e26dc-a0bc-4aeb-a355-659afe8238b0"));
    }

    #[test]
    fn a_refusal_is_a_failure_not_an_empty_result() {
        let stream = r#")]}'

20
[["wrb.fr","ogiZ0b",null,null,null,[7,null,[["type.googleapis.com/google.rpc.ErrorInfo",["PUBLIC_ERROR_UNUSUAL_ACTIVITY"]]]],"generic"]]
"#;
        assert_eq!(
            parse_stream(stream),
            Some(Reply::Failed(
                "the RPC refused the request (PUBLIC_ERROR_UNUSUAL_ACTIVITY)".to_owned()
            ))
        );
    }

    #[test]
    fn an_unreadable_body_is_not_a_verdict() {
        assert_eq!(parse_stream(")]}'\n\n2207\n[[\"wrb.fr\",\"ogiZ0b\",\"[[[\\\"tru"), None);
        assert_eq!(parse_stream(""), None);
    }
}
