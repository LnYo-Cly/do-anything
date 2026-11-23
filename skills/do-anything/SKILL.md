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
   - `browser.read` returns an element's text. After an action that starts a
     request, `browser.wait` for a locator or for `url_contains`: reading does
     not wait for text to change.
   - To assert or save in the same command, pass a whole step on stdin:
     `da session step <id> -f -` with `browser.read: { testid: status }`, an
     `expect:` list and/or `save: { name: $.text }`.
   - The snapshot is the first tool. `browser.screenshot --arg path=shot.png` is
     for what the tree cannot express (canvas, overlapping layout), not a
     default; there is no coordinate-based clicking.
3. `da session export <id> -o cases/flow.yaml --name "Create a note"` writes a
   deterministic case; transient refs become locators verified against the live
   page. `da session stop <id>` closes the session (`da session list` shows open
   sessions; idle ones are reaped after 30 minutes).
4. Edit the exported YAML to add the other layers, then `da lint cases/flow.yaml`
   and `da run cases/flow.yaml`. That is the replay: it must pass with no session
   and no `ref` left in the file.

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

## Guardrails

- **No secrets in cases.** Write `${env:NAME}` for a token, password or
  connection string; `da` reads it at run time and replaces the value with `***`
  in text output, `--output json` and every bundle file. A literal value in a
  case, a vars file or `--var` is not redacted.
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
