# flowy

Google Flow image generation from the command line, driven through the real UI in Safari.

```console
$ flowy generate "a lighthouse on a cliff at sunset"
Generating: a lighthouse on a cliff at sunset
1. https://flow-content.google/image/55f9c730-…?Expires=…&Signature=…
2. https://flow-content.google/image/69d7dd4f-…?Expires=…&Signature=…
```

Flow has no public API, so flowy drives the app the way a person does: it types the prompt into
the prompt box, presses generate, watches the project for the new frames, and prints their URLs.
The images come from a signed CDN URL, so `flowy download` fetches them with plain HTTP — no
cookies, no browser session.

## Requirements

- macOS with Safari, and the safari-driver extension bridge enabled in the Safari profile that is
  signed in to Google.
- A Flow project already created in that profile (Flow's UI, once).

## Commands

| Command | What it does |
| --- | --- |
| `flowy open` | Open Flow in the owned tab |
| `flowy status` | What the automation currently sees: URL, prompt box, buttons, images |
| `flowy generate "<prompt>"` | Generate, wait for the frames, print their URLs |
| `flowy images` | List the generated images in the current project |
| `flowy download [--index N] [--out DIR]` | Save images to `./out` (all, or one) |
| `flowy prompt "<text>"` | Type into the prompt box without submitting |
| `flowy fire` | Submit whatever the prompt box holds |
| `flowy reload` / `flowy focus` | Reload the tab / bring it to the front |
| `flowy eval "<script>"` / `flowy eval-file <path>` | Run a page script and print the JSON result |
| `flowy net-install` / `flowy net-dump [filter]` | Record the app's own requests, for reverse-engineering |

`generate` takes `--timeout` (per attempt, default 240s) and `--retries` (default 2).

## Why the trigger is not a click

The obvious implementation — `button[aria-label="Start generation"].click()` — does nothing, and
neither does dispatching `Enter` into the editor. Flow checks the event's trust flag:

```js
// flow-base-prompt-box.submit(event)
if (!flagA || !flagB || (event != null && event.isTrusted)) {
  startTheGeneration(event);
}
```

A synthetic event arrives with `isTrusted: false`, and WebKit does not let a page redefine it —
not on `Event.prototype`, and not on the event instance. Replaying the request against Flow's
`batchexecute` endpoint needs a payload the app keeps to itself.

What does work is the binding the framework installs for the button's `generate` output. That
listener calls `submit(event)` with whatever it is handed, so handing it a plain object whose
`isTrusted` is `true` passes the check. The listener only exists while the prompt box is being
built, so flowy hooks `addEventListener`, takes one round trip through the home route to rebuild
the component, and keeps the listener on `window` for the rest of the page's life.

An in-app route change is used deliberately: a real navigation would replace the document and
lose the hook.

## What the direct RPC would need

Flow's front end does the generation with a `batchexecute` POST to `rpcids=ogiZ0b`, and the
payload is small: model key, aspect ratio, seed, count, and the prompt.

```
[null,[[null,null,null,<seed>,3,"BELUGA",null,[null,22,null,null,null,"<project>",null,null,
  null,null,["<2048-character token>"]]]]]
```

That token is minted by reCAPTCHA Enterprise — the page calls
`www.google.com/recaptcha/enterprise/clr` at the moment generation is triggered. Every attempt
to send the call from outside the app comes back HTTP 200 with an RPC-level refusal:

```
[["wrb.fr","ogiZ0b",null,null,null,[7,null,[["type.googleapis.com/google.rpc.ErrorInfo",
  ["PUBLIC_ERROR_UNUSUAL_ACTIVITY"]]]],"generic"]]
```

Three variants were tried: the captured body verbatim, the same body with a token minted fresh
through `grecaptcha.enterprise.execute`, and that body with a new prompt and seed. All three
were refused, and the third test changed nothing but the token — so the refusal is not about
token freshness or the prompt. It is about the shape of the request the app builds: a streamed
`Request` on the `rt=c` protocol, preceded by the `ngNC2` call, with client state that a
standalone POST does not reproduce.

Reconstructing that pipeline is a much larger job than the UI path it would replace, and every
rejected attempt is itself an "unusual activity" signal against the account that owns the
project. `flowy` therefore drives the app's own submit path; `net-install` / `net-dump` stay as
diagnostics for anyone who wants to pick the thread up later.

## Why a run retries

Flow's image service fails requests often enough to matter — "Sorry, this image failed to
generate", or "This generation might violate our policies" for an entirely ordinary prompt. Both
are retryable through the card's own Retry button, which is an ordinary click with no trust
check. `flowy generate` watches for those notices and retries the card instead of reporting a
failure the first time it sees one.

## Reading the page while it is in the background

Safari stops delivering `requestAnimationFrame` to a hidden document, so an Angular page in a
background tab stops updating. flowy holds the parked callbacks and releases them on demand, in
short bounded bursts, at the top of every probe: an idle tab costs nothing, and a probe never
runs longer than its budget.

## Layout

- `src/page.rs` — the Safari bridge, the pinned engine, and the one owned Flow tab
- `src/ui.rs` — the background-render patch, the page probe, and `status`
- `src/generate.rs` — the prompt, the captured trigger, the retry loop, and downloads
- `src/browser.rs`, `src/state.rs` — the stable extension session id in `~/.flowy`
