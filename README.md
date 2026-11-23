# bateri-browser

The hidden Chromium helper behind bateri's browser element, and the crate the two talk through.

bateri draws a terminal. A browser element needs a real engine, which is far too large to live in
that process, so it runs here: a small macOS app (Chromium Embedded Framework, off-screen) that
bateri downloads, verifies, starts with a single-use key, and talks to. The helper has no window
(bar the developer tools', when asked for), no Dock icon and never asks to send notifications. Every page it draws reaches bateri as an `IOSurface` over a mach port; every
click, key and command goes the other way over a unix socket.

## What is in here

| crate | what it is |
|---|---|
| `ipc/` | The conversation: control messages (`Command`, `Event`), the socket handshake, frames over mach, the release notice (`Manifest`), and a wgpu import of a frame (feature `wgpu`). bateri depends on this crate by `rev`. |
| `helper/` | The app. `bateri-browser` is the browser process; `bateri-browser-helper` is what Chromium starts for its renderer, GPU and network processes; `bundle` lays out the `.app`. |
| `fake/` | A stand-in for bateri that starts the real app the way bateri does and drives it. The end-to-end tests are here. |

## Guarantees

- **No start without a key.** bateri passes a single-use key; the helper sends it back in its first
  message and bateri checks it, the protocol version and the peer's process id. A helper started by
  hand exits.
- **One way out to the network.** All traffic goes through a loopback proxy bateri runs
  (`--proxy-server` on the engine's command line, loopback not exempt). A profile's proxy identity
  is fixed per profile, so bateri tells profiles apart by it.
- **No requests of its own.** The engine asks for nothing no page asked for (no update checks, no
  time from Google, no Safe Browsing checks, which are the warnings about dangerous sites), so the
  addresses a person visits stay on their machines.
- **Chromium's cage stays closed.** Renderer and GPU processes run under Chromium's macOS sandbox.
  The engine's own command line is switched off (`command_line_args_disabled`), so `--no-sandbox`
  reaches nothing.
- **A closed set of addresses.** Only `http`, `https`, `about:blank` and `data:` load; frames inside
  a page may also be `about:srcdoc` and `blob:`. Redirects and subframes are checked too.
- **Nothing is granted.** Camera, microphone and permission prompts are refused; menus, dialogs,
  file choosers and downloads are questions to bateri, never answered by the helper.

## Build and test

You need macOS on Apple Silicon, a Rust toolchain, and `cmake` and `ninja` (`brew install cmake
ninja`). The first build downloads Chromium Embedded Framework (about 1 GB) and compiles its C++
wrapper; it takes a few minutes and is done once per target directory.

```sh
make check        # fmt, clippy -D warnings, fast tests (no engine starts)
make app          # target/debug/app-bundle/bateri-browser.app, signed
make check-heavy  # builds the app and runs the end-to-end tests against it
```

`make app` signs with the best identity in your keychain (Developer ID Application, else Apple
Development, else ad-hoc); `SIGN_ID=-` forces ad-hoc. Everything runs headless: the tests serve
pages from a stub proxy, read page events back through `document.title`, and never open a window.
Set `BATERI_BROWSER_TRACE=1` to make the helper log what it does to stderr.

## Releasing

`make release` builds in release mode, signs with a Developer ID identity, notarizes with the
keychain profile `bateri-notary` (`NOTARY_PROFILE` overrides it), staples, and writes the archive,
`SHA256SUMS` and `notice.json` to `dist/`. It pushes and publishes nothing. The version follows
Chromium's; `notice.json` is what bateri reads to find a newer helper.

## Licence

GPL-3.0-or-later. Chromium Embedded Framework is BSD-3-Clause; its licence and Chromium's
third-party notices ship inside the app (`Contents/Resources`). The standard Chromium build has no
proprietary codecs, so H.264 and AAC video do not play.
