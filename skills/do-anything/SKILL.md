---
name: do-anything
description: Verify a real end-to-end flow with the `da` CLI after changing a page, an API or a database schema. Covers exploring an unfamiliar page in a persistent browser session, exporting the steps as a replayable YAML case, asserting the UI, HTTP and database layers in that one case, and reading the failure evidence afterwards.
---

# Driving a real flow with `da`

`da` runs YAML cases against a service that is already running. One case can
drive a real Chrome page (`browser.*`), call the API (`http.request`), read
SQLite or PostgreSQL (`db.query`), start a program (`cli.run`) and observe a log
file or the page's network requests (`log.file`, `browser.network`). Reach for it
when a unit test cannot show that the flow works end to end — after changing a
page, a handler, a query or a schema. A pure function is still a unit test.

```sh
da --version   # the binary; installation is in docs/guide.md
da schema      # every registered step and observer, as a JSON Schema
```

If the flow needs an internal protocol that `da` does not ship, pass an
external plugin executable explicitly: `da --plugin ./internal-plugin schema`,
then use the same `--plugin` flag on `da lint`, `da run`, `da session` and
`da mcp`. A plugin registers steps, read-only observers and detachable sessions
over synchronous newline-delimited JSON-RPC 2.0, and inherits the same schema,
lint, session, report and secret-redaction behavior described here. A session
started with `--plugin` remembers the path, so later `da session` commands do
not repeat it.

## Two paths

**A case already exists.** `da lint <file-or-dir>` validates every file against
the parser and the registry, with line and column, and executes nothing: do it
first, and `da run <file-or-dir>` afterwards. Start the service yourself — `da`
never starts or stops it — and wait until it accepts requests. Use `--output
json` for one JSON object on stdout, and `--var name=value` / `--vars-file
env.yaml` instead of editing the case for each environment.

**No case yet: explore, export, replay.** `da session` keeps a browser alive
between commands and prints JSON by default.

1. `da session start` → a `session_id` (add `--preview-frames` — or
   `"preview_frames": true` in MCP `session_start` — to atomically write
   `#000000` input-masked `.da/sessions/<id>/preview.png` and redacted
   `preview.json` after UI steps without adding base64 images to LLM `content`).
   - Pause/resume for human takeover (login, 2FA, CAPTCHA, visual review) with
     `da session pause <id> [--reason "..."]` / `da session resume <id>` (or
     MCP `pause_for_human` / `resume`); queued agent steps fail closed while
     paused. Every `da mcp` `tools/call` also emits standard `structuredContent`
     alongside `content` text.
   - When built with default-off `--features live-preview`,
     `da session preview-bridge <id>` starts a `127.0.0.1`-only bearer-protected
     CDP Screencast + input-forwarding bridge scoped strictly to the session's
     owned tab (human-typed credentials cannot be retroactively redacted by
     `Secrets`).
2. Drive the page with the accessibility snapshot returned after every browser
   step:

   ```sh
   da session step <id> browser.open --arg url=http://127.0.0.1:3000
   da session step <id> browser.type --arg ref=e10 --arg text=alice
   da session step <id> browser.click --arg ref=e14
   ```

   - Address elements by `ref` from the snapshot of the **previous** command; a
     ref from an older snapshot is refused. When a snapshot says
     `truncated: true`, expand a subtree with `da session snapshot <id> --ref e7`.
   - Use the structured browser Computer Use primitives directly with `ref`s or
     locators: `browser.click`, `browser.type`, `browser.scroll` (`delta_x`/`delta_y`
     on viewport or container `ref`, or `into_view: true`), `browser.hover`,
     `browser.drag` (`from` and `to` `ref`s or locators), `browser.set_clipboard`,
     and `browser.read_clipboard` (clipboard steps require an isolated
     `da`-launched `headless: true` browser). All interactions stay strictly scoped
     to the owned CDP tab — never move the host OS cursor or touch unrelated windows.
   - `browser.read` returns an element's text. After an action that starts a
     request, `browser.wait` for a locator or for `url_contains`: reading does
     not wait for text to change.
   - To assert or save in the same command, pass a whole step on stdin:
     `da session step <id> -f -` with `browser.read: { testid: status }`, an
     `expect:` list and/or `save: { name: $.text }`.
   - The snapshot is the first tool. `browser.screenshot --arg path=shot.png` and
     explicit `at: { x, y, viewport }` coordinates (supported on `browser.click`,
     `browser.type`, `browser.scroll`, `browser.hover`, and `browser.drag`
     endpoints) are a last resort for `<canvas>` or WebGL surfaces that have no
     accessibility node; `da lint` warns on every `at` step, and `da session
     export` pins the observed `viewport` automatically.
   - When an accessibility tree is dense, an opaque `<canvas>` has no DOM
     nodes, or you want exploration-time data extraction, multi-step driving,
     or a verdict, use built-in multi-provider models (`da session start
     --model <model>` / `--model-endpoint <url>` / `--model-api-key-env <VAR>`
     / `--model-reasoning-effort <level>`, `DA_MODEL*`, or per-call `--model` /
     `--adapter` / `--endpoint` / `--api-key-env` / `--reasoning-effort`) or an
     external JSON-RPC hook (`--model-hook <exe>` or `--hook <exe>`):
     - `da session select-ref <id> --query "Save button" --act browser.click`
       asks `model/select_ref` to pick a snapshot `ref` and synthesizes its
       deterministic locator;
     - `da session locate <id> --query "Chart node" --click` sends an
       input-masked PNG + `viewport` to `model/locate`, reverse-synthesizes a
       deterministic locator when an HTML/UIA control is hit (`0` lint warnings),
       and falls back to viewport-checked `at: { x, y, viewport }` only on
       opaque `<canvas>`;
     - `da session judge <id> --rubric "No error banner"` (add `--screenshot` to
       attach a 1:1 input-masked viewport/window PNG on visual sessions) runs an
       exploration-time `model/judge` check over `Secrets`-redacted evidence
       (recorded in `steps.jsonl`, never exported to YAML);
     - `da session extract <id> --query "Order ID and total"` (optional
       `--schema`, `--screenshot`, `--save VAR`) extracts structured JSON via
       `model/extract` and optionally stores it into a session variable without
       exporting a `model.extract` step;
     - `da session act <id> --goal "Fill coupon and submit"` (optional
       `--max-steps N`, default `8`, max `50`; `--screenshot`) runs bounded
       autonomous `model/act_step` iterations that execute real steps
       (`step`, `locate_click`, `locate_type`) and synthesize deterministic
       locators so `da session export` compiles them cleanly into model-free
       YAML.
3. `da session export <id> -o cases/flow.yaml --name "Create a note"` writes a
   100% deterministic, model-free case; transient refs (including both endpoints
   of `browser.drag` and `browser.scroll`/`browser.hover` targets),
   model-located control hits, and steps executed during `act` compile into
   verified locators against the live page, and `model.*` diagnostic entries are
   omitted. `da session stop <id>` closes the session (`da session list` shows
   open sessions; idle ones are reaped after 30 minutes).
4. Edit the exported YAML to add the other layers, then `da lint cases/flow.yaml`
   and `da run cases/flow.yaml`. That is the replay: it must pass with no session
   and no `ref` left in the file.

### Structured, Compilable Computer Use loop

Prefer the 3-step structured loop (`da session` or `da mcp`) over raw
screenshot-and-pixel loops:

1. **Explore with snapshots (`ref`s):** drive clicks, typing, scrolling, hovering,
   drag-and-drop, and clipboard flows using accessibility `ref`s — zero pixels
   guessed, zero unrelated host windows touched. If the owned tab is closed
   externally, steps fail fast with exit code `4` (`Environment`).
2. **Compile to YAML (`da session export`):** `ref`s synthesize into `testid`,
   `role`+`name`, `text`, or CSS locators (structural paths are marked
   `# fragile:`), and `${env:NAME}` secrets stay parameterized.
3. **Lint and replay (`da lint` + `da run`):** validate schemas, secret wiring,
   and retryability (`browser.read_clipboard` allows `retry`; mutating steps
   do not), then replay deterministically across UI, API, and database layers.

## One case, three layers

```yaml
name: A note is saved and reaches the database
observe:
  net:
    browser.network: {}        # page requests land in observed.net.requests
steps:
  - browser.open:
      url: ${base}
  - browser.click:
      role: button
      name: Save
  - browser.wait:
      testid: form-status
  - browser.read:
      testid: form-status
    expect:
      - path: $.text
        equals: "Saved: alice"
  - http.request:              # the API sees the same record
      url: ${base}/api/notes
    expect:
      - path: $.status
        equals: 200
  - db.query:                  # read-only; PostgreSQL URL or sqlite:<path>
      url: ${env:DATABASE_URL}
      sql: SELECT title FROM notes WHERE title = $1
      params: ["alice"]
    expect:
      - path: $.row_count
        equals: 1
```

An observer is merged into every step result as `observed.<name>`, so an ordinary
`expect` path asserts it.

## Reading a failure

Exit codes: `0` every case passed, `1` a case failed an assertion or a step could
not finish, `2` command-line usage, `3` invalid case (including an unset
`${env:NAME}`), `4` environment (unreachable service, refused connection,
unreadable file), `5` timeout.

A failing case leaves `.da/failures/<run>/<case>/` (or `--bundle-dir`):
`report.json` names the failing step and carries each assertion's path, expected
and actual value; `browser/screenshot.png` and `browser/dom.html` show the page;
`meta.json` is written last, so a directory without it is incomplete. Read
`report.json` first. For evidence of passing cases too, run with
`--record <empty-dir>` and render it as one static HTML file:

```sh
da run cases/ --record evidence/ --trace-steps
da report evidence/                 # interactive offline HTML with SVG overlays & timeline
da report .da/failures/<run>        # or a single failure run
da report .da/sessions/<session-id> # or an exploration session with model telemetry
```

## RSI loop: verify a repair, not just a green run

When you edit the program or its cases and need to prove the edit did not
regress or weaken the suite, use the three RSI gates. They are deterministic and
need no model or network call.

```sh
# A trusted starting point: every case recorded, case files copied aside.
da run cases/ --record baseline-evidence/
cp -r cases/ baseline-cases/

# 1. Static Goodhart guard: parses both sides, executes nothing.
da lint cases/ --against baseline-cases/

# 2. Execute the candidate and record every case, passing and failing.
da run cases/ --record candidate-evidence/

# 3. Regression diff over the records; case positions may shift freely.
da compare baseline-evidence/ candidate-evidence/
```

`da lint --against` exits 3 for a deleted case, step or assertion; a weakened
matcher (`equals` to `contains`/`exists`, `contains` to `exists`); an added
`optional: true`; a widened `retry.timeout_ms`; a locator replaced with `at`
coordinates; or a case that active filters now exclude. Do not silence it by
editing the baseline. For a genuinely intentional spec change, pass
`--allow-weaken "the reason"` and say why in the delivery notes.

`da compare` matches cases by recorded `(case_file, case_name)`, not by the
`{position}-{safe_name}` directory prefix, and reports `regressed`, `fixed`,
`still_failing`, `still_passing`, `added` and `removed`, step and assertion
deltas, durations and `case_changed`. It exits 1 only when a case regressed; a
deleted case is left to `da lint --against`.

When a failure may be a flake, classify it before repairing it:

```sh
da run cases/suspicious.yaml --repeat 3
```

`--repeat N` runs every selected case N times and classifies each as
`stable_pass` (N/N passed), `stable_fail` (0/N) or `flaky` (1..N-1). It exits 0
only when every case is `stable_pass`; otherwise the worst exit code across
attempts wins (3, then 4, then 5, then 1). `--repeat N --record evidence/`
writes `attempt-1`, `attempt-2`, ... and `repeat-summary.json`, so
`da compare attempt-1/ attempt-2/` shows what changed between attempts.

Route the result by exit code: 2 fix the command line; 3 fix the case at the
reported line (or record an intentional `--allow-weaken` reason); 4 fix the
environment; 5 reproduce with `--repeat` before touching a timeout; 1 use
`da compare` to separate a new regression from an old failure, and `--repeat` to
separate a deterministic failure from a flake. Never weaken `src/` or a test to
turn a gate green.
## On a phone

`android.*` drives one Android device over `adb`, in a build that enables the
`android` feature:

```sh
cargo install --path . --features android   # or: cargo build --features android
export DA_ADB=/path/to/adb                  # default: adb on PATH
export DA_ANDROID_SERIAL=emulator-5554      # needed when several devices answer
```

The steps are `android.launch` (a `package` and/or an `activity`),
`android.tap` (`x`, `y`), `android.input_text` (`text`), `android.screenshot`,
`android.dump` (the UI tree, optionally filtered by `text` or `resource_id`) and
`android.tap_on`, which reads the tree once and taps the node it found. Reading
the tree costs about two seconds on a real device and does not get cheaper with
compression, so only `dump` and `tap_on` do it: prefer `tap` with coordinates a
previous `dump` gave you when a case taps several times in a row. Every step
takes `timeout_ms` (default 30000) and a step that fails while the device still
answers writes a screenshot and the tree beside the case file.

```yaml
name: Battery setting
steps:
  - android.launch: { package: com.android.settings }
  - android.dump: { text: Battery }
  - android.tap_on: { text: Battery }
  - android.input_text: { text: "50" }
```

## On a desktop app

`desktop.*` drives native GUI windows on Windows (UI Automation), Linux (AT-SPI2
over D-Bus + X11/XTEST), and macOS (`AXUIElement` + `CGEvent` + `screencapture`)
in a build that enables `--features desktop`:

```sh
cargo build --features desktop
# Headless Linux CI: xvfb-run -a dbus-run-session -- da run cases/desktop.yaml
```

Explore with `da session start` → `desktop.launch` (or `desktop.attach`, which
marks the run `local_only: true` and never kills the external process) →
`da session snapshot <id>` (`e1`, `e2`, … refs) → `desktop.click` / `desktop.type`
/ `desktop.press` / `desktop.wait` / `desktop.read` / `desktop.screenshot` →
`da session export <id>`. `Edit` and password fields are masked solid black in
screenshots by default (`mask_inputs: true`). On macOS, grant **Accessibility**
and **Screen Recording** permissions to the terminal/runner; on Linux, pure
Wayland supports semantic AT-SPI2 steps while screenshots, `desktop.press`, and
`at:` coordinates require X11, XWayland, or `xvfb-run`.

## Guardrails

- **No secrets in cases.** Write `${env:NAME}` for a token, password,
  clipboard secret or connection string; `da` reads it at run time and
  replaces the value with `***` in text output, `--output json`, session logs,
  and every bundle file (`browser.set_clipboard` returns only `characters`). A
  literal value in a case, a vars file or `--var` is not redacted.
  `browser.set_clipboard` and `browser.read_clipboard` require an isolated
  `da`-launched `headless: true` browser and fail fast with exit code `4`
  (`Environment`) when `connect` or `headless: false` is used, so `${env:NAME}`
  secrets and host OS clipboard contents never cross the session boundary.
- **Lint before run**, and never weaken `src/` or a test to make a case pass.
- **Bundles and reports are evidence, not publishing material.** They hold page
  text and step arguments; a report embeds screenshots unless you pass
  `--shareable`. Review before sharing, and keep `.da/` and evidence directories
  out of version control.
- **Locators**: `testid`, then `role` + `name`; `text` for static copy;
  `css`/`xpath` only for structure, narrowed with `within` or `nth`. Without
  `nth` a locator must match exactly one element, and a step that matches
  several fails immediately with the candidates. A step the export marks
  `# fragile:` (or lists in its `fragile` array) is a structural CSS path — fix
  it before trusting it.
- **The service is yours.** `da` starts and stops only the programs a `cli.run`
  step names and the browser a case opens (a Chrome or Chromium already
  installed; `DA_CHROME` names one). `browser.open` with `connect` drives a
  browser you started with a debug port instead.

Details are in `docs/guide.md`: [sessions](../../docs/guide.md#interactive-sessions-da-session),
[browser steps](../../docs/guide.md#browser-steps), [desktop steps](../../docs/guide.md#desktop-steps),
[failure bundles](../../docs/guide.md#failure-bundles), [external plugins](../../docs/guide.md#external-plugins),
[secrets](../../docs/guide.md#secrets), [exit codes](../../docs/guide.md#exit-codes).
