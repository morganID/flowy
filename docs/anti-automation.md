# Why Flow is hard to automate

Notes from building `flowy` against `flow.google.com` in October 2026. Every claim below was
measured against the live app; the numbers are from that session.

The short version: Flow does not rely on one defence. It stacks five, and each one is aimed at a
different layer, so getting past one only reveals the next. Only the first is a *check* in the
usual sense — the rest are consequences of how the app is built and how Google scores traffic.

```
   ┌──────────────────────────────────────────────────────────────┐
   │ 1  trusted event      the button ignores synthetic clicks     │  page JS
   ├──────────────────────────────────────────────────────────────┤
   │ 2  reCAPTCHA token    a ~2.5 KB token minted per request      │  page JS + Google
   ├──────────────────────────────────────────────────────────────┤
   │ 3  risk scoring       the RPC is refused after the fact       │  server
   ├──────────────────────────────────────────────────────────────┤
   │ 4  request coupling   the call only means something in a      │  protocol
   │                       stream, after its precursors            │
   ├──────────────────────────────────────────────────────────────┤
   │ 5  engineering        minified, no devtools, no stable        │  build
   │     opacity           handles to the app's own objects        │
   └──────────────────────────────────────────────────────────────┘
```

## 1. The button checks that the event is real

`flow-base-prompt-box.submit(event)` in the app's own code:

```js
if (!flagA || !flagB || (event != null && event.isTrusted)) {
  startGeneration(event);
}
```

With `flagA` and `flagB` both true in a normal session, the third clause decides, and a
script-dispatched click arrives with `isTrusted: false`. Measured consequences:

- `button.click()`, a full pointer sequence (`pointerdown`/`mousedown`/`pointerup`/`mouseup`/
  `click` with `detail: 1`), `requestSubmit`, and Enter dispatched into the editor all do nothing.
- The same synthetic clicks *do* work elsewhere on the page — the settings popover opens, the
  prompt box clears — so this is not a blanket "untrusted events are ignored" policy. The gate is
  on generation specifically.
- `isTrusted` cannot be faked in WebKit. The own accessor on the event is `configurable: true`,
  so `Object.defineProperty(event, 'isTrusted', {get: () => true})` succeeds — and the engine
  still reports `false`. Patching `Event.prototype` does nothing either, because the property is
  installed per instance.
- `navigator.userActivation` *is* fakeable (its `isActive` / `hasBeenActive` are writable own data
  properties) but that does not help: the guard reads the event, not the activation state.

The gap that made automation possible: the guard trusts *the object it is handed*, and the
framework's binding for the button's `generate` output passes that object through unchanged. Call
the binding with `{isTrusted: true}` and `submit` is satisfied. That is what `flowy` does.

## 2. Every request carries a reCAPTCHA Enterprise token

The generation itself is a `batchexecute` POST to `rpcids=ogiZ0b`:

```
[null,[[null,null,null,<seed>,3,"BELUGA",null,
  [null,22,null,null,null,"<project>",null,null,null,null,["<2510-char token>",1]],
  [[["a ceramic teapot on a windowsill"]]],null,null,null,"<uuid>","<uuid>"]]
```

Evidence that the token is reCAPTCHA Enterprise and not something else:

- At the moment generation fires, the page POSTs a protobuf to
  `www.google.com/recaptcha/enterprise/clr?k=6LdsFiUsAAAAAIjVDZcuLhaHiDn5nnHVXVRQGeMV`.
- The token matches what `grecaptcha.enterprise.execute(siteKey, {action})` returns on that page:
  same `0cAFcWeA…` prefix, same 2382–2510 character band (the length varies per call; the token in
  the payload above is 2510, a fresh call produced 2382).
- The page loads `recaptcha/enterprise.js?trustedtypes=true&render=<siteKey>` and keeps an
  `recaptcha/enterprise/anchor` iframe open.

The app reaches it through a small wrapper that resolves to
`grecaptcha.enterprise.execute(siteKey, {action})`, with the action supplied by the caller.

## 3. The server scores the request, and refuses it afterwards

Sending the call from outside the app returns **HTTP 200** with an RPC-level error:

```
[["wrb.fr","ogiZ0b",null,null,null,[7,null,[["type.googleapis.com/google.rpc.ErrorInfo",
  ["PUBLIC_ERROR_UNUSUAL_ACTIVITY"]]]],"generic"]]
```

Three variants, all refused:

| Attempt | What changed | Result |
| --- | --- | --- |
| replay the captured body | nothing | `UNUSUAL_ACTIVITY` |
| fresh token, new prompt and seed | token, prompt, seed | `UNUSUAL_ACTIVITY` |
| fresh token, everything else identical | token only | `UNUSUAL_ACTIVITY` |

The third row is the informative one: with the payload byte-identical apart from a token minted
seconds earlier, the answer is the same. So the refusal is not token expiry, not the prompt, and
not a stale nonce. It is a judgement about the request as a whole — which is what "risk scoring"
means, as opposed to validation.

Note the shape of the failure: HTTP 200. A client that only checks status codes sees success.

## 4. The call only means something inside the app's request pipeline

The generation POST is not a plain form submission:

- The body arrives as a `Request` object, not as a string in `init.body`. A recorder that only
  looks at `init.body` therefore sees the URL and an empty body; the payload only became visible
  after hooking the `Request` constructor itself.
- The URL carries `rt=c`, the streaming-response protocol, and a monotonically increasing
  `_reqid`.
- It is preceded by `ngNC2` (`tools/PINHOLE/projects/<id>`) and, around project load, by
  `NfrxTb` / `ve2Lsc` / `DTaVef`.
- It carries the `at` token from `WIZ_global_data.SNlM0e` and the build label `bl` from `cfb2h`,
  plus `f.sid`, `source-path`, and `x-same-domain: 1`.

The request headers themselves are unremarkable — `content-type` and `x-same-domain` only — so the
coupling is in the payload and the surrounding calls, not in a secret header.

## 5. The app is built so you cannot reach its own objects

Getting a stable handle on the app's internals is harder than reading its network traffic:

- The front end is ~3.4 MB of minified Wiz/Closure code, no source maps, module aliased to `_`.
- `window.ng` does not exist, and `element.__ngContext__` is a **number**, not the view — Angular's
  dev-mode context registry is compiled out, so component instances are not reachable from the DOM.
- The bundle keeps its own references to `fetch`, `Request`, and `addEventListener` from module
  load. Patching `window.fetch` after boot is invisible to it; a recorder only works if it is
  installed before the bundle executes.
- CSP is strict enough that `setAttribute('onclick', …)` throws and the page fires `cspreport`
  beacons, so inline handlers are not an option.
- `safari_navigate` returns once the document has loaded, which is already too late to inject
  before the bundle runs — the prompt box exists by then.

## Why the difficulty is deliberate, and where it stops

None of this is paranoia for its own sake. Image generation costs GPU time, and a free tier
attached to a Google account is worth abusing. The stack is the same one Google applies across its
AI products: prove a human is present at the expensive action, attach a scored token, and only then
spend the compute.

It stops short of one thing: the guard reads `event.isTrusted` off whatever object reaches
`submit`. That is a check on the *caller's honesty*, not on the browser's provenance, and it is why
a UI-driven client works at all. A stricter design would re-read the trust flag at the RPC boundary
or bind the request to a server-issued handle — the second is effectively what layer 4 already
does for anyone calling from outside.

## What this means in practice

- **UI automation: viable.** The app performs the request with its own code, its own token, and its
  own pipeline. `flowy` only decides *when* and *with what prompt*.
- **A standalone client: not viable** without reproducing the streaming pipeline, and each failed
  attempt is itself an `UNUSUAL_ACTIVITY` signal against the account. That trade is not worth it
  for a tool that already works in ~30 seconds.
- **Detectability of the automation itself is low.** `navigator.webdriver` is `false` through the
  extension bridge, and the interaction happens inside a real, signed-in Safari profile.

### Consequence for the hybrid design

The plan was direct RPC first, UI as fallback. Layer 3 rules that out: the direct path is refused
even with a token minted seconds earlier and the rest of the payload byte-identical, so it would
never be the fast path — it would be a guaranteed failure followed by the fallback, at the cost of a
flagged attempt on every run. The hybrid is therefore not worth building in that direction.

The half that *is* worth building is the reverse split: **UI for the trigger, RPC for the reads.**
Generation needs the trusted event and the scored token; polling for results does not. The read
RPCs (`NfrxTb`, `ve2Lsc`) are ordinary authenticated `batchexecute` calls with no reCAPTCHA in the
payload, so a run could fire through the UI and then fetch finished images over RPC instead of
scraping `src` attributes from the DOM.

That is now how `flowy generate` works, and the reply it uses is the generation RPC itself rather
than a separate read: `ogiZ0b` streams its result, and the stream closes only when the request is
over, so the reply *is* the completion signal. Measured on one project, both paths end to end:

| Path | Wall clock |
| --- | --- |
| generation RPC reply | 25–27 s |
| DOM with its settle window | 29 s |

The difference is the 2 s debounce and the DOM serialisation. The reply also does not care whether
the card has painted, which matters when the tab is in the background or the list virtualises.

