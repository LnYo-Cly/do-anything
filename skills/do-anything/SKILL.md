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

## Two paths

**A case already exists.** `da lint <file-or-dir>` validates every file against
the parser and the registry, with line and column, and executes nothing: do it
first, and `da run <file-or-dir>` afterwards. Start the service yourself — `da`
never starts or stops it — and wait until it accepts requests. Use `--output
json` for one JSON object on stdout, and `--var name=value` / `--vars-file
env.yaml` instead of editing the case for each environment.

**No case yet: explore, export, replay.** `da session` keeps a browser alive
between commands and prints JSON by default.

1. `da session start` → a `session_id`.
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
     and `browser.read_clipboard`. All interactions stay strictly scoped to the
     owned CDP tab — never move the host OS cursor or touch unrelated windows.
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
3. `da session export <id> -o cases/flow.yaml --name "Create a note"` writes a
   deterministic case; transient refs (including both endpoints of
   `browser.drag` and `browser.scroll`/`browser.hover` targets) compile into
   verified locators against the live page. `da session stop <id>` closes the
   session (`da session list` shows open sessions; idle ones are reaped after
   30 minutes).
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
da run cases/ --record evidence/
da report evidence/            # writes evidence/report.html
da report .da/failures/<run>   # or a single failure run
```

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

## Guardrails

- **No secrets in cases.** Write `${env:NAME}` for a token, password,
  clipboard secret or connection string; `da` reads it at run time and
  replaces the value with `***` in text output, `--output json`, session logs,
  and every bundle file (`browser.set_clipboard` returns only `characters`). A
  literal value in a case, a vars file or `--var` is not redacted, and
  clipboard values preexisting on an externally connected browser (`connect`)
  before `da` starts did not come from `${env:NAME}` and stay local-only
  (same boundary as `connect` cookies/storage).
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
[browser steps](../../docs/guide.md#browser-steps), [failure bundles](../../docs/guide.md#failure-bundles),
[secrets](../../docs/guide.md#secrets), [exit codes](../../docs/guide.md#exit-codes).
