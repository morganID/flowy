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
  signed in to Google. Enable it in **one** profile only: the bridge is a single port, so several
  profiles means several workers racing for it.
- A Flow project already created in that profile (Flow's UI, once).
- That profile pinned with `flowy profile "<name>"`. Without a pin the extension resolves a
  profile itself and a run can quietly drive the wrong account.

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
| `flowy profile [name]` | Show or pin the Safari profile flowy drives |
| `flowy tabs` | List the tabs the extension can see, and which one flowy uses |
| `flowy eval "<script>"` / `flowy eval-file <path>` | Run a page script and print the JSON result |
| `flowy net-install` / `flowy net-dump [filter]` | Record the app's own requests, for reverse-engineering |

`generate` takes `--timeout` (per attempt, default 240s), `--retries` (default 2), and `--no-rpc`
(read the DOM only). It says which path answered: `via the generation RPC` or `via the DOM`.

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
[null,[[null,null,null,<seed>,3,"BELUGA",null,
  [null,22,null,null,null,"<project>",null,null,null,null,["<2510-char token>",1]],
  [[["<prompt>"]]],null,null,null,"<uuid>","<uuid>"]]]
```

That token is minted by reCAPTCHA Enterprise — the page POSTs a protobuf to
`www.google.com/recaptcha/enterprise/clr` at the moment generation is triggered. Every attempt
to send the call from outside the app comes back HTTP 200 with an RPC-level refusal:

```
[["wrb.fr","ogiZ0b",null,null,null,[7,null,[["type.googleapis.com/google.rpc.ErrorInfo",
  ["PUBLIC_ERROR_UNUSUAL_ACTIVITY"]]]],"generic"]]
```

Three variants were tried: the captured body verbatim, the same body with a token minted fresh
through `grecaptcha.enterprise.execute` plus a new prompt and seed, and — the decisive one — a
fresh token with **nothing else changed**. All three were refused. So the refusal is not about
token freshness or the prompt. It is about the shape of the request the app builds: a streamed
`Request` on the `rt=c` protocol, preceded by the `ngNC2` call, with client state that a
standalone POST does not reproduce.

Reconstructing that pipeline is a much larger job than the UI path it would replace, and every
rejected attempt is itself an "unusual activity" signal against the account that owns the
project. `flowy` therefore drives the app's own submit path; `net-install` / `net-dump` stay as
diagnostics for anyone who wants to pick the thread up later.

The full write-up of the five layers — the trusted-event gate, the scored token, the risk
scoring, the request coupling, and why the app's internals are unreachable — is in
[docs/anti-automation.md](docs/anti-automation.md).

## Where the images come from

The trigger goes through the UI, but the *results* are read from the generation RPC's reply.
`ogiZ0b` answers on the streaming protocol, and its stream closes only when the request is over,
so the reply is a definitive completion signal — no settle window, no guessing whether a card has
finished painting.

To see it, `generate` installs a recorder at document start (the app keeps its own `fetch`
reference from module load, so a patch added later is invisible to it) and takes the reply whose
sequence number is newer than the mark recorded just before the click. The reply carries each
image URL twice with different signatures, so images are deduplicated by id.

The DOM stays as the fallback: when the recorder is absent, when it never saw the request, or
when the reply is late. `--no-rpc` forces that path. Measured on the same project:

| Path | Wall clock |
| --- | --- |
| generation RPC | 25–27 s |
| DOM (settle window) | 29 s |

The saving is the 2 s debounce and the DOM serialisation, not the generation itself.

A trigger that does not submit is now caught instead of waited out: a live submit clears the
prompt box and sends the request, so when neither happens within 6 s the binding belonged to a
destroyed component — `generate` rebuilds the prompt box and fires once more, then fails with a
message rather than sitting out the timeout.

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
- `src/net.rs` — the request recorder, and the reader for the generation RPC's reply
- `src/browser.rs`, `src/state.rs` — the pinned Safari session id and profile in `~/.flowy`
- `docs/anti-automation.md` — the security research behind the design
