# do-anything guide

The complete reference for `da`: installation options, building from source, the case file format, the built-in steps, failure bundles, output and exit codes. For a quick introduction, see the [README](../README.md).

## Contents

- [Installation](#installation)
- [Build](#build)
- [Test](#test)
  - [Self-test suite](#self-test-suite)
- [Usage](#usage)
  - [Parallel runs](#parallel-runs)
  - [The service under test](#the-service-under-test)
  - [Observers](#observers)
    - [Observers and parallel runs](#observers-and-parallel-runs)
  - [Sessions](#sessions)
  - [Interactive sessions (`da session`)](#interactive-sessions-da-session)
  - [Model Context Protocol (`da mcp`)](#model-context-protocol-da-mcp)
  - [Classification and filtering](#classification-and-filtering)
  - [Linting](#linting)
  - [Secrets](#secrets)
  - [Schema](#schema)
- [Case files](#case-files)
  - [Variables](#variables)
  - [Assertions](#assertions)
  - [Optional steps](#optional-steps)
  - [Retrying read-only steps](#retrying-read-only-steps)
  - [Cleanup with `finally`](#cleanup-with-finally)
  - [`http.request`](#httprequest)
  - [`cli.run`](#clirun)
  - [`db.query`](#dbquery)
  - [Browser steps](#browser-steps)
  - [Android steps](#android-steps)
  - [Desktop steps](#desktop-steps)
  - [`log.file`](#logfile)
- [Failure bundles](#failure-bundles)
- [Run records and HTML reports](#run-records-and-html-reports)
- [Output](#output)
- [Exit codes](#exit-codes)

## Installation

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/LnYo-Cly/do-anything/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/LnYo-Cly/do-anything/main/install.ps1 | iex
```

The scripts pick the build for your system and CPU (x86_64 or Arm64), download the latest release, check it against `SHA256SUMS`, and install `da` without administrator rights: to `~/.local/bin` on Linux and macOS, and to `%LOCALAPPDATA%\Programs\da` on Windows, which is added to your user `PATH`. On Linux and macOS the script prints the line to add when the directory is not on `PATH` yet.

If `raw.githubusercontent.com` is slow or blocked on your network, fetch the same scripts from jsDelivr:

```sh
curl -fsSL https://cdn.jsdelivr.net/gh/LnYo-Cly/do-anything@main/install.sh | sh
```

```powershell
irm https://cdn.jsdelivr.net/gh/LnYo-Cly/do-anything@main/install.ps1 | iex
```

Options, set as environment variables:

| Variable | Meaning |
| --- | --- |
| `DA_VERSION` | Release to install, for example `v0.1.0`. Default: the latest. |
| `DA_INSTALL_DIR` | Directory that receives `da`. |
| `DA_DOWNLOAD_URL` | Base URL of a mirror or proxy that serves the release files as `<base>/<tag>/<file>`. The archive and `SHA256SUMS` both come from it, so use one you trust. Finding the latest release still contacts `github.com` unless `DA_VERSION` is set. |
| `DA_NO_MODIFY_PATH` | Windows only: leave the user `PATH` unchanged. |

You can also download an archive from [GitHub Releases](https://github.com/LnYo-Cly/do-anything/releases) yourself, extract it, and put `da` on your `PATH`. Each release includes a `SHA256SUMS` file.

## Build

Requires Rust 1.89 or newer (edition 2024).

```sh
cargo build
```

The binary is written to `target/debug/da` (`target\debug\da.exe` on Windows).
The `http` feature provides the `http.request` step, `cli` provides `cli.run`,
`log` provides the `log.file` observer, `db` provides `db.query`, `browser`
provides the `browser.*` steps and `mcp` provides `da mcp`; these six are
enabled by default. The `android` feature provides the [`android.*`
steps](#android-steps) and is off by default, because it is only useful on a
machine with an Android device: build it with `cargo build --features android`.
The `desktop` feature provides the [`desktop.*` steps](#desktop-steps) and is
also off by default, because it drives a local graphical desktop session: build
it with `cargo build --features desktop`.
The `browser` feature adds about 0.5 MiB to the release
binary and needs a Chrome or Chromium installed on the machine; it never
downloads a browser. The `mcp` feature adds about 54 KiB to a build with no
drivers and about 82 KiB to the default build; it has no dependency and no
async runtime. Build a subset with
`--no-default-features --features http,cli,log,db`, build only one driver with
`--no-default-features --features cli`, or build nothing extra with
`--no-default-features`.

## Test

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test
```

The integration tests in `tests/cli.rs` run the compiled binary against a local
HTTP server that each test starts itself. They never reach the network.

The browser tests in `tests/browser.rs` launch the Chrome or Chromium already
installed on the machine and drive pages served by the same local test server.
When no browser can be started they print why and skip; set
`DA_TEST_BROWSER=1` to make that a failure instead, which is what CI does.

The Android tests in `tests/android.rs` need no device for their schema, lint and
missing-adb checks, and those run on every machine. The tests that drive a
device through `adb` are gated on `DA_TEST_ANDROID` the same way: with the
variable unset they print why and skip, and with `DA_TEST_ANDROID=1` a missing
device fails the suite.

The desktop tests in `tests/desktop.rs` run when `--features desktop` is
enabled (`cargo test --features desktop --test desktop`). Schema, lint, and
non-Windows `#88` stub checks run everywhere; on Windows the suite compiles a
temporary WinForms fixture with `csc.exe` and exercises `da session` snapshots,
exports, input-masked failure screenshots, `at:` coordinate pinning, and
closed-window error classification.

### Self-test suite

`tests/self/*.yaml` are cases that test `da` with `da`: every step is a
`cli.run` that starts the binary under test. `tests/self_suite.rs` runs them with
the compiled binary, so the existing `cargo test` covers them on Linux, macOS and
Windows without a separate CI job:

```sh
cargo test --test self_suite
da run tests/self --var da=target/debug/da    # the same suite, by hand
```

`--var da=<path>` tells every case which binary to start. The cases that the
suite calls are fixtures in `tests/self/fixtures/`; they use the `.case`
extension so that the recursive directory scan of `tests/self` does not pick
them up as tests. Run one explicitly, `da run tests/self/fixtures/passing.case`,
to see what it does. The fixtures use `da` itself as the program under test, so
the suite needs neither the network nor a system command such as `echo` or
`sleep`. One case runs a case that is meant to fail, so a manual run writes a
failure bundle under `.da/failures`, which git ignores; add
`--var bundles=<directory>` to put it somewhere else.

`tests/self/observe-log.yaml` exercises the `log.file` observer. It needs a
program that appends a line to a file without a shell, and there is exactly one
such program every checkout of this repository has: `git`. The case calls
`git config --file <file> --add da.line "order created"`, which appends to the
file it names on Linux, macOS and Windows. The file it writes is ignored by
`tests/self/.gitignore`. A machine without `git` on `PATH` fails that one case
with an environment error (exit 4); the rest of the suite does not use it.

## Usage

```sh
da run cases/login.yaml
da run cases/
da run cases/ --var token=abc --var user=alice
da run cases/ --vars-file envs/local.yaml --vars-file envs/ci.yaml
da run cases/ --layer api --tag smoke
da run cases/ --tag smoke --skip-tag slow
da run cases/ --jobs 4
da run cases/ --bundle-dir /tmp/da-failures
da run cases/ --no-bundle
da run cases/ --record evidence/
da run cases/ --record evidence/ --trace-steps
da report evidence/
da report evidence/ -o evidence.html --shareable
da --output json run cases/
da lint cases/
da lint cases/ --var token=abc
da lint cases/ --vars-file envs/ci.yaml
da --output json lint cases/
da schema
da --output json schema > da-case.schema.json
```

`da run` accepts files and directories. A directory is searched recursively for
`.yaml` and `.yml` files, which are run in path order. Every case file, every
step and every observation is validated before the first case runs: if anything
is invalid, no case runs and the exit code is 3. A step that fails an assertion
stops its own case; the remaining steps of that case are reported as skipped and
the other cases still run. A step marked `optional: true` is the exception: it is
reported as `skipped` and the case continues (see
[Optional steps](#optional-steps)).

`--var name=value` defines a variable for every case of the run and may be
repeated. It overrides a variable of the same name in the case's `vars:`. A
`--var` without `=` or with an empty name is a usage error (exit 2).

`--vars-file <file>` reads variables from a YAML mapping and may be repeated.
The keys are variable names; the values are strings, numbers or booleans, the
same way `vars:` reads them, so `port: 8080` becomes the text `8080`. A file is
applied in the order it appears on the command line, and a later source
overrides an earlier one: a case's `vars:`, then each `--vars-file`, then
`--var`. A file that does not exist, is not a mapping, or holds a value that is
not a string, number or boolean is a usage error (exit 2) that names the file
and, where the parser knows it, the line and column:

```yaml
# envs/staging.yaml
base: https://staging.example.test
user: alice
retries: 3
```

```sh
da run cases/ --vars-file envs/staging.yaml
```

Use it to run one set of cases against a local server, a staging deployment and
CI without editing the case files:

```sh
da run cases/ --vars-file envs/local.yaml
da run cases/ --vars-file envs/staging.yaml
da run cases/ --vars-file envs/ci.yaml --var base=http://127.0.0.1:8080
```

`da lint` accepts `--vars-file` and `--var` as well, and validates the cases
with the same values a run would use.

`--bundle-dir <directory>` changes where failure bundles are written (see
[Failure bundles](#failure-bundles)); `--no-bundle` turns them off.

### Parallel runs

`--jobs N` runs up to N cases at the same time, with N at least 1. The default
is 1, which runs them one after another. Cases are independent by design —
variables and `save` belong to one case, so no case sees another one's values —
and a set that waits on a service benefits directly: cases that each wait a
second finish in about a second when they run at the same time, instead of
adding up.

```sh
da run cases/ --jobs 4
```

The result does not depend on the job count. Cases are reported in the order
they were discovered, in text output and in JSON, and the exit code follows the
same rules; only the durations change. Every failing case still gets its own
failure bundle, because the name of a bundle directory carries the case's
position in the run. A value below 1 and a value that is not a whole number are
usage errors (exit 2), and `--jobs` belongs to `da run`; `da lint` does not have
it.

Do not raise `--jobs` when the cases are not independent. Two cases that touch
the same state — one test account, one row, one file under a fixed path — can
pass alone and fail together, and the failure looks like a race in the service.
Run that group with `--jobs 1` and a filter of its own, such as `--tag slow`,
and give the rest of the cases a run of their own.

A listener receives the events of one case in order — `CaseStarted`, the
events of its steps, then `CaseFinished` — but the events of different cases
interleave, so a listener must group them by the case index every event carries
instead of assuming the order of the run. Nothing in `da` subscribes to the bus
today; this is the contract for the plugins that will.

### The service under test

A case talks to a service that is already running: `da` sends the requests and
starts the programs a step names, but it never starts, restarts or stops the
service itself. Running a case against a server therefore takes four steps of
your own shell:

1. Start the service, in its own terminal or as a background process.
2. Wait until it accepts requests.
3. Run the cases.
4. Stop the service when the run is over. `da` leaves it running.

Pass the address with `--var` instead of writing a port into the case file, so
the same cases run against a local server and a deployed environment:

```sh
da run cases/ --var base=http://127.0.0.1:8080
```

A run that starts before the service is listening reports an environment error
(exit 4) whose message names the connection failure, and no assertion is
evaluated. That is how a wrong target is told apart from a wrong expectation: an
unreachable service is never a failed assertion.

### Observers

An interface that answers `201` has not necessarily done what it promised. An
**observer** reads another layer while a step runs — a log file, the requests a
page sends, or a database later — so a case can assert on the evidence the step
left behind. The built-in observers are `log.file` for a file and, with the
`browser` feature, `browser.network` for the page's requests ([Observing the
network](#observing-the-network)).

`observe:` is optional. It is a mapping: the key is the name you choose for the
observation, and the value is exactly one registered observer and its arguments,
written the same way as a step:

```yaml
name: Order is logged
observe:
  app_log:
    log.file:
      path: ${log_dir}/app.log
steps:
  - http.request:
      method: POST
      url: ${base}/orders
    expect:
      - path: $.status
        equals: 201
      - path: $.observed.app_log.text
        contains: "order created"
```

`text` is the new log text as one string, so `contains` asks whether it holds
that substring. `lines` is a list, and `contains` on a list asks whether one
element is exactly the expected value — not whether an element contains it. A
line such as `request method=POST ... order created` is therefore not a match for
`contains: "order created"` on `lines`. Use `text` for a substring, and a path
into one element, such as `$.observed.app_log.lines[0]`, with `equals` when the
exact line matters.

The collected value is merged into **every step result** of the case as
`observed.<name>`, before the assertions are evaluated. An ordinary `expect` path
is enough, no new assertion syntax is needed, and a failure bundle's `report.json`
carries the same values, redacted like everything else.

| Rule | Meaning |
| --- | --- |
| Name | Becomes a key of `observed`, so it uses lowercase letters, digits, `.`, `_` and `-`. A repeated name is a duplicate mapping key and the YAML parser refuses it with its line and column. |
| One observer | An entry names exactly one observer and its arguments. An unknown observer or arguments that do not satisfy its schema is a case error (exit 3), reported by `da lint` and `da run` with the line and column of the node. |
| Variables | Arguments accept `${name}` and `${env:NAME}` and are resolved once, when the case starts. A value a step `save`s is not available to an observation. |
| Isolation | Each case creates its own observation, but the file it reads is shared state: a case that observes the same log file as another case sees its lines too. See [Observers and parallel runs](#observers-and-parallel-runs). |
| Steps | The observer starts before each step and is collected after it, so it reports what that one step changed. |
| Read-only | Observers only read. The built-in ones open files for reading and write nothing. |
| Observer errors | A file that cannot be read is an environment error (exit 4); the message names the observation and the observer. |
| No result | A step that fails or times out produces no result, so its observation is not collected: there is nothing to merge it into. |

#### Observers and parallel runs

An observation is only as isolated as the file it reads. When several cases
observe one log file and run at the same time with `--jobs`, the lines another
case adds during a step are part of this case's `observed.<name>.text` and
`lines` too. The observed value then depends on how many cases run at once, and
an assertion such as `contains: "order created"` can pass on another case's line.
That is the "one file under a fixed path" shared state of
[Parallel runs](#parallel-runs), reached through an observer.

Give each case its own log file, and let the program under test write to the file
the case names. A case takes the directory from the environment and chooses the
file name itself:

```yaml
name: Customer order is logged
vars:
  log_name: customer
observe:
  app_log:
    log.file:
      path: ${log_dir}/${log_name}.log
steps:
  - http.request:
      method: POST
      url: ${base}/orders
      json: { sku: SKU-CUSTOMER }
    expect:
      - path: $.observed.app_log.text
        contains: "SKU-CUSTOMER"
```

A second case is the same file with `log_name: status` and its own SKU, and the
service writes `${log_dir}/customer.log` and `${log_dir}/status.log` — one worker
or log target per case, chosen so that a request cannot reach the other file.
Then `da run cases/ --jobs 2` observes only its own case's lines, and the result
is the same as with `--jobs 1`. `base` and `log_dir` come from a `--vars-file`, as
in [Usage](#usage).

If the program can only write one file, do not raise `--jobs` for those cases:
run them with `--jobs 1` and a `--tag` of their own, as
[Parallel runs](#parallel-runs) says. A shared log does not fail by itself — it
makes the observed value depend on the other cases, which is worse, because the
assertion can still pass.

Every built-in step returns a JSON object. If a plugin's step returns another
value and the case declares observations, the result is wrapped so that nothing
is lost: the original value moves to `value` and the observations stay under
`observed`, as in `{"value": 5, "observed": {"app_log": {"lines": []}}}`.

**Writing an observer.** The engine offers the same registration path to
built-in and external extensions. An observer declares a name, a description, a
JSON Schema for its arguments and a `validate` function that enforces the same
contract; `da schema` publishes both under `$defs` and inside `observe`. Its
factory creates one instance per case, and the instance implements two calls:
`start` before a step and `collect` after it, which returns the JSON value the
step result shows under `observed.<name>`. An environment failure from either
call is an environment error (exit 4); arguments that do not satisfy `validate`
are a case error (exit 3). Both name the observation and the observer kind. The
contract lives in `src/core/observer.rs`; `src/drivers/log.rs` is a complete
example, and `tests/architecture.rs` keeps the engine free of any concrete
observer.

### Sessions

A step that talks to a browser, a device or a connection needs the same
resource in several steps: the page opened by step one is the page step two
clicks, and the network observer has to read that page. A **session** is that
resource. It belongs to one case, is created the first time a step asks for it,
and is closed when the case ends.

The engine knows nothing about the resource. A plugin declares its own type,
implements `Session`, and stores it under a kind it chooses:

```rust
use do_anything::core::session::{Session, Sessions};
use do_anything::core::registry::StepError;

struct Page {
    // A connection, a child process, a device handle, ...
}

impl Session for Page {
    fn close(&mut self) -> Result<(), String> {
        // Release the page. Return a message if that fails.
        Ok(())
    }
}

/// Opens the resource, or explains why it could not.
fn launch_page() -> Result<Page, String> {
    // ...
}

fn open(
    _args: &serde_json::Value,
    sessions: &mut Sessions,
) -> Result<serde_json::Value, StepError> {
    let page = sessions
        .get_or_create("browser.page", launch_page)
        .map_err(StepError::Execution)?;
    // Use `page` for this step; later steps ask for the same kind.
    Ok(serde_json::json!({ "opened": true }))
}
```

`get_or_create` takes the concrete type as a generic parameter and the kind as
a `&'static str`. The first call runs the factory; every later call of the same
case returns the value already stored. A factory that returns an error leaves
no slot behind, so a later step may try again. Asking for one kind with two
different types is an error, not a panic: one kind holds one concrete type for
the whole case.

A kind follows the step name rule: lowercase letters, digits, `.`, `_` and
`-`. It is the plugin's own namespace and does not appear in a case file, so
two plugins can choose their own kinds without coordinating.

An observer receives the same `Sessions` value in `start` and `collect`:

```rust
use do_anything::core::observer::{Observer, ObserverError};
use do_anything::core::session::Sessions;
use serde_json::Value;

impl Observer for NetworkCapture {
    fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
        Ok(())
    }

    fn collect(&mut self, sessions: &mut Sessions) -> Result<Value, ObserverError> {
        let page = sessions
            .get::<Page>("browser.page")
            .map_err(ObserverError::Environment)?
            .ok_or_else(|| ObserverError::Environment("no page was opened".to_string()))?;
        // Read the requests the page made during the step.
        Ok(serde_json::json!({ "requests": [] }))
    }
}
```

`start` runs before the step, so on the first step the session may not exist
yet; `collect` runs after it, so that is where a resource the step opened is
available. `get` returns `Ok(None)` for a kind the case never opened and an
error for a kind held by another type.

| Rule | Meaning |
| --- | --- |
| One set per case | The runner creates a new `Sessions` for every case. Cases that run at the same time with `--jobs` cannot see each other's sessions, so a plugin must not keep the resource in a global static. |
| Created on first use | The factory runs when a step or an observation first asks for the kind. A case that never asks creates nothing. |
| Closed at the end | The runner closes every open session in reverse creation order, whatever the case did: passed, failed an assertion, errored, timed out, or stopped before the remaining steps. Each session is closed exactly once. |
| Close failure | The message names the session kind. A case that would have passed becomes an environment error (exit 4). A case that already failed or errored keeps its own status and problem, and the close error is attached to the report. |
| Where it appears | Text output adds a `close:` line under the case. JSON adds `close_errors` to the case object, and a failure bundle's `report.json` carries the same field. Close messages are redacted like every other string. |
| Fallback | `Sessions` also closes what is still open when it is dropped, for a panic or an early return. The normal path does not rely on it, and a fallback close cannot be reported because the report is already gone. |

The built-in `http.request`, `cli.run`, `db.query` and `log.file` use no
session, so their behavior is unchanged: each step still opens and closes what
it needs within the step. The `browser.*` steps use one browser session per
case (see [Browser steps](#browser-steps)); the contract exists for the drivers
and observers that need a resource across steps.

### Interactive sessions (`da session`)

`da session` lets an agent or user explore a service step-by-step across
multiple CLI calls while keeping live resources (such as a detached browser)
open between invocations, then exports the verified sequence as a replayable
YAML case file. Unlike `da run` (where `Session::close` always tears down
resources on every case exit path), `da session` detaches live handles on disk
under `.da/sessions/<id>/` (overridable with `--sessions-dir`) and closes them
on `da session stop` or idle timeout reap.

Every `da session` subcommand prints one JSON object to stdout by default
(except `da session export --yaml`, which prints raw YAML). A non-blocking lock
on the session directory rejects concurrent operations on the same session ID.

| Subcommand | Purpose & Key Options |
| --- | --- |
| `da session start` | Create `.da/sessions/<id>/`, reap expired sessions (`--idle-timeout-ms`, default `1800000`), optionally attach declared read-only observers (`--observe SPEC`), print `session_id` |
| `da session step <id> ...` | Reattach resources, run one step (`<step> --arg k=v`, `--use` + `--with`, `--step`, or `-f <file\|->`, optional `--save name=jsonpath`), update state, detach; returns `snapshot` after browser steps |
| `da session snapshot <id>` | Capture the current accessibility tree without running a step (`--ref <id>`, `--max-depth`, `--max-nodes`) |
| `da session export <id>` | Emit deterministic YAML case (`-o <file>`, `--name <title>`, `--include-reads`, `--yaml`); skips pure `browser.read` without `expect`/`save` by default |
| `da session stop <id>` | Close all resources in the session and remove `.da/sessions/<id>/` and any temporary browser profile |
| `da session list` | List active sessions and reap any session idle beyond `--idle-timeout-ms` (`0` reaps all idle sessions) |

#### End-to-end example

Start a session, open a login page, use snapshot `ref` targets (`e5`, `e9`, `e11`,
then `e3` after navigation) with `${env:DEMO_PASS}` for the secret, read a
greeting with an assertion, export to YAML, lint, replay with `da run`, and stop:

```console
$ da session start
{"dir":".da/sessions/s-1794974416388-44376-0","idle_timeout_ms":1800000,"ok":true,"reap_errors":[],"reaped":[],"session_id":"s-1794974416388-44376-0"}

$ da session step s-1794974416388-44376-0 browser.open --arg url=http://127.0.0.1:54030/login
{"ok":true,"session_id":"s-1794974416388-44376-0","snapshot":{"max_depth":12,"max_nodes":150,"node_count":9,"title":"Login","tree":[{"name":"Sign in","ref":"e1","role":"heading"},{"children":[{"children":[{"name":"Username","ref":"e4","role":"StaticText"}],"ref":"e3","role":"LabelText"},{"name":"Username","ref":"e5","role":"textbox"},{"children":[{"name":"Password","ref":"e7","role":"StaticText"}],"ref":"e6","role":"LabelText"},{"name":"Password","ref":"e8","role":"textbox"},{"name":"Sign In","ref":"e9","role":"button"}],"ref":"e2","role":"form"}],"truncated":false,"url":"http://127.0.0.1:54030/login"},"step":{"assertions":[],"duration_ms":12207,"error":null,"name":"browser.open","result":{"title":"Login","url":"http://127.0.0.1:54030/login"},"status":"passed"}}

$ da session step s-1794974416388-44376-0 browser.type --arg ref=e5 --arg text=alice
{"ok":true,"session_id":"s-1794974416388-44376-0","snapshot":{"max_depth":12,"max_nodes":150,"node_count":10,"title":"Login","tree":[{"name":"Sign in","ref":"e1","role":"heading"},{"children":[{"children":[{"name":"Username","ref":"e4","role":"StaticText"}],"ref":"e3","role":"LabelText"},{"children":[{"name":"alice","ref":"e6","role":"StaticText"}],"name":"Username","ref":"e5","role":"textbox","value":"alice"},{"children":[{"name":"Password","ref":"e8","role":"StaticText"}],"ref":"e7","role":"LabelText"},{"name":"Password","ref":"e9","role":"textbox"},{"name":"Sign In","ref":"e10","role":"button"}],"ref":"e2","role":"form"}],"truncated":false,"url":"http://127.0.0.1:54030/login"},"step":{"assertions":[],"duration_ms":203,"error":null,"name":"browser.type","result":{},"status":"passed"}}

$ export DEMO_PASS="s3cr3t-pw"
$ da session step s-1794974416388-44376-0 -f - <<'EOF'
browser.type:
  ref: e9
  text: "${env:DEMO_PASS}"
EOF
{"ok":true,"session_id":"s-1794974416388-44376-0","snapshot":{"max_depth":12,"max_nodes":150,"node_count":11,"title":"Login","tree":[{"name":"Sign in","ref":"e1","role":"heading"},{"children":[{"children":[{"name":"Username","ref":"e4","role":"StaticText"}],"ref":"e3","role":"LabelText"},{"children":[{"name":"alice","ref":"e6","role":"StaticText"}],"name":"Username","ref":"e5","role":"textbox","value":"alice"},{"children":[{"name":"Password","ref":"e8","role":"StaticText"}],"ref":"e7","role":"LabelText"},{"children":[{"name":"•••••••••","ref":"e10","role":"StaticText"}],"name":"Password","ref":"e9","role":"textbox","value":"•••••••••"},{"name":"Sign In","ref":"e11","role":"button"}],"ref":"e2","role":"form"}],"truncated":false,"url":"http://127.0.0.1:54030/login"},"step":{"assertions":[],"duration_ms":387,"error":null,"name":"browser.type","result":{},"status":"passed"}}

$ da session step s-1794974416388-44376-0 browser.click --arg ref=e11
{"ok":true,"session_id":"s-1794974416388-44376-0","snapshot":{"max_depth":12,"max_nodes":150,"node_count":3,"title":"Welcome","tree":[{"name":"Dashboard","ref":"e1","role":"heading"},{"children":[{"name":"Welcome back, Alice","ref":"e3","role":"StaticText"}],"ref":"e2","role":"paragraph"}],"truncated":false,"url":"http://127.0.0.1:54030/welcome"},"step":{"assertions":[],"duration_ms":231,"error":null,"name":"browser.click","result":{},"status":"passed"}}

$ da session step s-1794974416388-44376-0 -f - <<'EOF'
browser.read:
  ref: e3
expect:
  - path: "$.text"
    equals: Welcome back, Alice
EOF
{"ok":true,"session_id":"s-1794974416388-44376-0","snapshot":{"max_depth":12,"max_nodes":150,"node_count":3,"title":"Welcome","tree":[{"name":"Dashboard","ref":"e1","role":"heading"},{"children":[{"name":"Welcome back, Alice","ref":"e3","role":"StaticText"}],"ref":"e2","role":"paragraph"}],"truncated":false,"url":"http://127.0.0.1:54030/welcome"},"step":{"assertions":[{"actual":"Welcome back, Alice","comparison":"equals","expected":"Welcome back, Alice","message":null,"passed":true,"path":"$.text"}],"duration_ms":743,"error":null,"name":"browser.read","result":{"text":"Welcome back, Alice"},"status":"passed"}}

$ da session export s-1794974416388-44376-0 -o login-flow.yaml --name "Login flow"
{"case_name":"Login flow","exported_steps":5,"fragile":[],"ok":true,"path":"login-flow.yaml","session_id":"s-1794974416388-44376-0","skipped_steps":[],"yaml":"name: Login flow\nsteps:\n  - browser.open:\n      url: http://127.0.0.1:54030/login\n  - browser.type:\n      name: Username\n      role: textbox\n      text: alice\n  - browser.type:\n      name: Password\n      role: textbox\n      text: \"${env:DEMO_PASS}\"\n  - browser.click:\n      testid: submit-btn\n  - browser.read:\n      testid: greeting\n    expect:\n      - path: \"$.text\"\n        equals: Welcome back, Alice\n"}
```

Exported `login-flow.yaml` replaces transient `ref` values with live-verified
stable locators while keeping `${env:DEMO_PASS}` untouched (never storing any
resolved secret value inside `.da/sessions/`):

#### Session observers and step audit log

`da session start` accepts repeatable `--observe SPEC` flags to attach declared
read-only observers (such as `log.file` or `browser.network`) across every
subsequent `da session step` call:

```sh
da session start --observe app_log=log.file:path=app.log --observe net=browser.network
da session start --observe 'app_log=log.file:{"path":"app.log"}'
```

Syntax errors exit 2 (`usage`); unknown observer names, invalid observer
arguments, or unresolved `${env:NAME}` secrets fail with exit 3 before creating
any session directory. Observer arguments are resolved with `${env:NAME}`
references (not step-saved `${var}` variables). Attached observers expose
`$.observed.<name>` in each step result before `expect` and `save` run, and `da
session export` writes the declared `observe:` block (preserving unexpanded
`${env:NAME}` references) into the exported YAML case.

Each entry in `.da/sessions/<id>/steps.jsonl` records:

- `started_at_ms`: Unix epoch milliseconds when the step started;
- `duration_ms`: execution duration in milliseconds;
- `result`: the redacted step result JSON (or `null` when absent), while `raw_args` preserves unexpanded `${env:NAME}` and `${var}` placeholders (and masks inline sensitive literals such as raw cookie values to `***`, which `da session export` refuses to emit silently).

```yaml
name: Login flow
steps:
  - browser.open:
      url: http://127.0.0.1:54030/login
  - browser.type:
      name: Username
      role: textbox
      text: alice
  - browser.type:
      name: Password
      role: textbox
      text: "${env:DEMO_PASS}"
  - browser.click:
      testid: submit-btn
  - browser.read:
      testid: greeting
    expect:
      - path: "$.text"
        equals: Welcome back, Alice
```

A step that clicked explicit coordinates is exported with the viewport it ran at,
so the case cannot silently replay at another size, and it is listed in the
export report's `fragile` array with `"reason": "explicit_coordinates"` and that
`viewport`:

```yaml
  # fragile: explicit coordinates recorded at viewport 1280x800; replay this case at the same size
  - browser.click:
      at:
        x: 100
        y: 50
        viewport:
          width: 1280
          height: 800
```

Validate and replay it deterministically without interactive refs, then clean up:

```console
$ da lint login-flow.yaml
Checked 1 file: 0 errors.

$ da --output json run login-flow.yaml --no-bundle
{"bundle_errors":[],"cases":[{"bundle":null,"close_errors":[],"duration_ms":7979,"error":null,"file":"login-flow.yaml","layer":null,"name":"Login flow","project":null,"status":"passed","steps":[{"assertions":[],"duration_ms":2690,"error":null,"name":"browser.open","result":{"title":"Login","url":"http://127.0.0.1:54030/login"},"status":"passed"},{"assertions":[],"duration_ms":296,"error":null,"name":"browser.type","result":{},"status":"passed"},{"assertions":[],"duration_ms":250,"error":null,"name":"browser.type","result":{},"status":"passed"},{"assertions":[],"duration_ms":354,"error":null,"name":"browser.click","result":{},"status":"passed"},{"assertions":[{"actual":"Welcome back, Alice","comparison":"equals","expected":"Welcome back, Alice","message":null,"passed":true,"path":"$.text"}],"duration_ms":75,"error":null,"name":"browser.read","result":{"text":"Welcome back, Alice"},"status":"passed"}],"suite":null,"tags":[]}],"errors":[],"ok":true,"summary":{"cases":1,"cases_errored":0,"cases_failed":0,"cases_passed":1,"duration_ms":8032,"filtered":0,"steps":5,"steps_errored":0,"steps_failed":0,"steps_passed":5,"steps_skipped":0}}

$ da session stop s-1794974416388-44376-0
{"errors":[],"ok":true,"session_id":"s-1794974416388-44376-0"}
```

#### Snapshots, `ref` lifecycle, and locator synthesis

- **Compact AXTree format**: snapshots read Chrome's accessibility tree, skip
  or unwrap `ignored` nodes, assign compact sequential `e1`, `e2`, … refs, and
  preserve element state such as `value`, `checked`, `disabled`, and `expanded`.
  Defaults cap output at `max_depth: 12` and `max_nodes: 150`, setting
  `truncated: true` when trimmed; expand sections with `da session snapshot <id> --ref <eN>`.
- **Use the latest snapshot**: every browser step returns a fresh `snapshot`
  whose refs replace the previous table for that session. Passing a stale or
  unknown `ref` fails immediately and asks for a fresh `snapshot`. Static `ref`
  fields are invalid in case files (`da lint` and `da run` refuse them).
- **Main-document scope for snapshots & `ref`s**: `da session snapshot` reads
  the main document's accessibility tree (showing `<iframe>` elements as
  `Iframe` nodes without descending into child documents), and 7-tier `ref`
  locator synthesis targets main-document elements. Inside `da session step`,
  steps that use explicit `within` locators still enter same-origin `<iframe>`
  and `<frame>` elements and export verbatim into replayable YAML.
- **7-tier locator synthesis on export**: each `ref`-targeted step resolves the
  live element (stepping up from `StaticText` or wrapper nodes to the enclosing
  interactive element) and picks the first unique, live-verified locator in
  order: (1) `testid` (`data-testid` or `testid_attribute`), (2) `role` + `name`,
  (3) associated `<label>` / `placeholder` as `role` + `name`, (4) exact
  `text`, (5) stable non-generated `id` (`css: #<id>`), (6) nearest landmark
  `within` + non-generated class selector, (7) structural CSS path marked
  `# fragile:` in the YAML and listed in the export JSON's `fragile` array.
  Generated hashes/utility tokens and digit-heavy IDs/classes are penalized or
  skipped.
- **Structured, Compilable Computer Use**: instead of screenshot-and-pixel guessing
  loops, explore the page with `da session` (or `da mcp`) using the structured
  accessibility snapshot and `ref`s across `browser.click`, `browser.type`,
  `browser.scroll`, `browser.hover`, `browser.drag`, `browser.set_clipboard`, and
  `browser.read_clipboard`; export deterministic YAML with `da session export`
  (compiling `ref`s into 7-tier locators and pinning any `at` coordinates to the
  observed `viewport`); then run `da lint` and `da run` for repeatable multi-layer
  replay. All interactions stay target-scoped over CDP (never moving the host OS
  cursor or touching unrelated windows), and any `${env:NAME}` secret written to
  the browser clipboard is automatically redacted across CLI, JSON, session logs,
  and failure bundles.
- **Dialogs**: `da session` steps use the same JavaScript-dialog handling as
  `da run`. A `ref`-targeted `browser.click`, `browser.type`, `browser.upload`,
  `browser.wait`, `browser.read`, `browser.scroll`, `browser.hover`, or
  `browser.drag` (when `from.ref` or `to.ref` is used) has no `dialog` argument,
  so an unexpected `alert`, `confirm`, `prompt` or `beforeunload` is dismissed
  immediately and fails that step with the dialog type and its redacted message.
  Take a fresh snapshot and retry after the page settles. Locator-targeted
  `browser.open`, `browser.click`, `browser.type`, `browser.press`,
  `browser.wait`, `browser.scroll`, `browser.hover` and `browser.drag` steps
  accept `dialog` in a session exactly as they do in a case file.
- **No `retry` in a session step**: `retry` polling belongs to the case runner
  (`da run` / `da lint`). A `da session step` with a `retry` block is rejected
  as an invalid-input error (exit 3) instead of silently ignoring the policy;
  send one attempt from the session, or move the polling step into a case file.
  Session steps still share the same YAML/JSON step schema, `use`, `with`,
  `expect` and `save` behavior as case steps.
- **Secrets & cleanup**: resolved `${env:...}` secret strings are redacted from
  JSON outputs and never written into session logs or handles. `da session stop`
  and idle reaping close detached Chrome instances and delete both
  `.da/sessions/<id>/` and Chrome's temporary profile directory.

### Model Context Protocol (`da mcp`)

Some coding-agent hosts have no shell tool and can only reach the outside world
through the [Model Context Protocol](https://modelcontextprotocol.io/). `da mcp`
serves the same transport-neutral session protocol as `da session` over standard
input and output, so such a host can explore a service step by step and export a
replayable case without running a command. The server is synchronous, adds no
dependency and needs no async runtime, and it lives behind its own `mcp` cargo
feature.

A host starts it as an ordinary stdio server, for example:

```json
{
  "mcpServers": {
    "da": {
      "command": "da",
      "args": ["mcp"],
      "cwd": "."
    }
  }
}
```

The process runs until standard input reaches end of file. Standard output
carries responses only, and each one is flushed before the next request is read,
so no diagnostic can be mistaken for a response. `--sessions-dir` and
`--idle-timeout-ms` mean the same thing they mean for `da session`, and the
sessions it creates are the `.da/sessions/<id>/` directories `da session list`
and `da session export` already use.

#### Framing and methods

Every message is JSON-RPC 2.0. Two framings are accepted: one JSON object per
line, and the `Content-Length` framing the Language Server Protocol uses. A
response always uses the framing of the request it answers, so a host that
speaks only one of them needs no negotiation.

| Method | Result |
| --- | --- |
| `initialize` | `protocolVersion` (echoed when this server knows it, otherwise the newest one it speaks), `capabilities.tools`, `serverInfo` and `instructions` |
| `notifications/initialized` | Nothing: JSON-RPC forbids answering a notification |
| `ping` | `{}` |
| `tools/list` | The fixed six tools with their compact `inputSchema` |
| `tools/call` | The result of one tool |

An unknown method answers `-32601`, a body that is not JSON answers `-32700`,
and a `tools/call` that names no tool answers `-32602`.

#### The six tools

The list never changes with the build. One tool per registered step would grow
with every driver and would differ between feature combinations, which spends a
model's context on descriptions it cannot use.

| Tool | Arguments | Result |
| --- | --- | --- |
| `session_start` | none | `session_id` of a new session |
| `step` | `session_id`, `use`, optional `with`, `expect`, `save` | The step result, its assertions and, after a browser step, a fresh `snapshot` |
| `snapshot` | `session_id`, optional `ref`, `max_depth`, `max_nodes` | The accessibility tree without running a step |
| `export` | `session_id`, optional `name`, `include_reads`, `out` | The YAML case, byte for byte what `da session export --yaml` writes |
| `run` | `paths`, optional `vars`, `jobs` | The same JSON report `da --output json run` prints |
| `session_stop` | `session_id` | Closes every resource and removes the session directory |

`step` is the only generic step tool: `use` names a registered step, `with`
holds the arguments a case file nests under that name, and `expect` and `save`
mean what they mean in a case file. The step goes through the case parser, so a
session step and a case step accept exactly the same shape:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"step","arguments":{"session_id":"s-1794974416388-44376-0","use":"http.request","with":{"method":"GET","url":"http://127.0.0.1:8080/health"},"expect":[{"path":"$.status","equals":200}]}}}
```

#### Correcting a refused step

When the arguments do not satisfy the step's schema, the tool result is an error
whose payload carries that step's complete JSON Schema under `arguments_schema`,
so the model can fix the call instead of guessing. A `use` that names no
registered step lists the steps the build has in `available_steps` instead, which
also tells a host why a step is missing:

```json
{
  "ok": false,
  "session_id": "s-1794974416388-44376-0",
  "error": {"kind": "invalid_input", "message": "`cli.run` invalid arguments: missing field `command`"},
  "arguments_schema": {"type": "object", "required": ["command"], "properties": {"command": {"type": "string"}}},
  "hint": "`with` must satisfy `arguments_schema` for `cli.run`; fix the arguments and call `step` again with the same `session_id`."
}
```

#### Secrets

Every tool output passes through the same `Secrets` redaction as the command
line: a value read through `${env:NAME}` becomes `***`, in results, in error
messages and in the exported case, including its percent-encoded and base64
forms. The exported YAML keeps the `${env:NAME}` reference, never the value.

### Classification and filtering

A case may describe where it belongs, so one repository can hold interface,
command-line, UI and future database cases side by side:

```yaml
name: Login returns a token
project: shop
suite: smoke
layer: api
tags: [smoke, fast]
steps:
  - http.request:
      url: http://127.0.0.1:8080/health
    expect:
      - path: $.status
        equals: 200
```

| Field | Type | Meaning |
| --- | --- | --- |
| `project` | string | Project the case belongs to. |
| `suite` | string | Suite the case belongs to, for example a smoke or regression suite. |
| `layer` | string | Layer the case exercises. Free-form: this project suggests `unit`, `api`, `cli`, `ui`, `integration`, `e2e` and `db`, and a game, chip or systems project uses its own values. |
| `tags` | list of strings | Cross-cutting labels. Duplicates are a case error. |

All four fields are optional and are never interpolated: write the value you
mean, and a `${name}` inside one stays literal. An empty value is a case error
(exit 3), as is a repeated tag.

`da run` selects cases with `--project`, `--suite`, `--layer`, `--tag` and
excludes them with `--skip-tag`; each option may be repeated:

```sh
da run cases/ --layer api
da run cases/ --project shop --project billing
da run cases/ --layer api --layer cli --tag smoke
da run cases/ --tag smoke --skip-tag slow
```

- Values of one option are alternatives: `--layer api --layer cli` runs both
  layers.
- Different options must all match: `--layer api --tag smoke` runs cases that
  are in the api layer **and** carry the smoke tag.
- `--skip-tag` wins over every other option: a case with any skipped tag does
  not run.
- Matching is exact and case-sensitive.
- A case that does not declare a field is not selected by a filter on that
  field: with `--layer api`, a case without `layer` does not run.

Every discovered file is still parsed and validated, whether or not the filters
select it. A filter never lets an invalid case file pass: if a filtered-out file
is broken, the run exits 3 and no case runs.

When the filters select no case, `da run` exits 2 and the message names the
options that were used and the `project`, `suite`, `layer` and tag values the
discovered cases actually have, so a typo is visible without running anything.
A run without filters is not affected by this rule.

Filtered-out cases do not appear in the result; the summary counts them as
`filtered` (text) or `summary.filtered` (JSON).

### Linting

`da lint` takes the same files and directories as `da run`, and the same
`--var name=value` and `--vars-file` options. It discovers, loads and validates
every case with the same preflight that `da run` uses, but it does not execute a
step, write a failure bundle or emit run events. A case that lint accepts is a
case that `da run` accepts before it starts; problems that only appear while a
step runs, such as an unreachable target, are outside lint's scope.

Unlike a run, lint reports every problem in every file instead of stopping at
the first one. Text output writes `file: message` for each problem and ends with
a summary. A YAML grammar error and every structural problem of a case file — a
step, an assertion, a `save` entry or a variable — name the line and column of
the node they belong to and keep the parser's source snippet, including its line
breaks and caret line, so the reported location can be read directly:

```text
cases/login.yaml: line 6 column 9: step 1: assertion 1: unknown field(s) equal; use one of equals, not_equals, contains, exists
 --> <input>:6:9
  |
4 |       command: da
5 |     expect:
6 |       - path: $.exit_code
  |         ^ step 1: assertion 1: unknown field(s) equal; use one of equals, not_equals, contains, exists
7 |         equal: 0
  |
cases/logout.yaml: line 2 column 1: unknown field `naem`, expected one of name, vars, project, suite, layer, tags, observe, steps
 --> <input>:2:1
  |
1 | name: Logout
2 | naem: typo
  | ^ unknown field `naem`, expected one of name, vars, project, suite, layer, tags, observe, steps
3 | steps:
4 |   - cli.run:
  |
Checked 2 files: 2 errors.
```

Problems that need the registry — an unregistered step name, step arguments that
do not match the step's schema, an undefined variable, or an observation that
names an unknown observer — name the node the user has to edit and carry its line
and column, because the parser cannot know them. An unregistered step points at
its name, an argument error at the step's argument mapping, and an undefined
variable at the value that holds the reference:

```text
cases/login.yaml: line 12 column 33: step 2: undefined variable `${acces_token}`; define it under `vars:` or save it in an earlier step
  --> <input>:12:33
   |
10 |   - http.request:
11 |       url: ${base}/me
12 |       headers: { Authorization: "Bearer ${acces_token}" }
   |                                 ^ step 2: undefined variable `${acces_token}`; define it under `vars:` or save it in an earlier step
Checked 1 file: 1 error.
```

```text
cases/login.yaml: line 4 column 5: observation `app_log`: `log.fil` is not a registered observer; registered observers: `log.file`
 --> <input>:4:5
  |
2 | observe:
3 |   app_log:
4 |     log.fil:
  |     ^ observation `app_log`: `log.fil` is not a registered observer; registered observers: `log.file`
5 |       path: app.log
  |
```

`--output json` writes exactly one object:

```json
{
  "ok": false,
  "files": 2,
  "errors": [
    {
      "file": "cases/login.yaml",
      "message": "line 12 column 33: step 2: undefined variable `${acces_token}`; define it under `vars:` or save it in an earlier step"
    }
  ],
  "cases": [
    {
      "file": "cases/smoke.yaml",
      "name": "Login returns a token",
      "project": "shop",
      "suite": "smoke",
      "layer": "api",
      "tags": ["smoke", "fast"]
    }
  ]
}
```

A located message also carries the source snippet, with its line breaks escaped
as `\n` inside the JSON string; the example shows the first line only.

A case that is valid but relies on something fragile is not an error. A step can
declare a warning for its own arguments — explicit coordinates are the first one,
see [Coordinates](#coordinates) — and `da lint` reports it while the case still
passes. Text output writes `file: warning: message` to standard error, the
standard-output summary counts the warnings, and `--output json` adds a
`warnings` array beside `errors`:

```json
{
  "ok": true,
  "files": 1,
  "errors": [],
  "warnings": [
    {
      "file": "cases/editor.yaml",
      "message": "line 6 column 7: step 2: `browser.click` uses explicit coordinates `at`; ..."
    }
  ],
  "cases": []
}
```

A warning never changes the exit code, and a step that fails validation never
warns: the warning describes arguments that are accepted but fragile. The
`warnings` key is omitted when there is nothing to warn about, so a report
without warnings keeps exactly the shape shown above.

`cases` lists every case that parsed, with its classification, so an agent can
discover the available values without running anything. `project`, `suite` and
`layer` are `null` when the case does not declare them.

The exit code is 0 when every case is valid and 3 when anything is invalid,
including a path that cannot be read and a directory with no case files. Use
`da lint tests/self --var da=target/debug/da` to check the self suite without
running it.

### Secrets

A value that `da` read through `${env:NAME}` is treated as a secret. The rule
follows the source, not a declaration: nothing has to be marked, so a value
cannot be forgotten. Values from `vars:`, `--vars-file` and `--var` are not
secrets and stay readable, because they are already visible in a file or in the
command line.

Before anything is written, every occurrence of every secret is replaced with
`***`. That covers:

- text output on standard output and standard error, including the actual value
  of a failed assertion and the message of a step error;
- `--output json`, as one object with the same replacement inside strings,
  object keys, numbers, booleans and `null`;
- the `report.json` and `meta.json` of every failure bundle;
- every error message, including one produced before a case runs, such as a
  rejected step argument.

A string or an object key keeps its own text and is redacted in place. A number,
a boolean or `null` has no text of its own to replace, so its JSON text is
redacted and the result becomes a string: with the secret `1`, the number `10`
becomes `"***0"`, and with the secret `true`, the boolean `true` becomes
`"***"`. A token that contains no secret keeps its type.

The replacement happens in the two places where `da` writes results — the
renderer and the bundle writer — not in each driver, so a new driver or reporter
cannot bypass it. A failure bundle keeps the copy of the case file as written:
it holds `${env:NAME}` references and no values.

```console
$ export DA_TOKEN=s3cr3t-token
$ da run cases/
FAIL cases/login.yaml [Login returns a token] 6 ms
  http.request: 1 of 2 assertions failed
    $.body.token equals "t-123" (actual "***")
  bundle: .da/failures/1763000000123-4242/0-login-returns-a-token
Summary: 1 case (0 passed, 1 failed, 0 errors, 0 filtered), 2 steps (1 passed, 1 failed, 0 errors, 0 skipped), 7 ms
```

A value of at least 8 bytes is also recognized in two encoded forms:

- **Percent-encoded bytes.** Each byte of the value may appear as itself or as
  `%XX`, with hexadecimal digits in either case; a space may also appear as
  `+`. This is the form a URL query string commonly uses. The matcher works byte
  by byte, so it does not depend on which characters an encoder chose to
  escape. The whole matched span becomes one `***`.
- **base64.** Both the standard and URL-safe alphabets are recognized, with or
  without `=` padding. The value may be encoded by itself or embedded in a
  longer base64 document; the result depends on where the value starts, so all
  three byte offsets are checked. The base64 characters at both ends that share
  bits with the value are replaced as well, together with any padding, so no
  character carrying value bits is left behind.

Encoded forms are only searched for values of at least 8 bytes. A shorter value
is replaced only when it appears exactly, because byte-by-byte matching would
otherwise rewrite too much nearby text. The exact form is still replaced for
every value.

Redaction has limits that matter when you choose a value:

- **Short secrets hide unrelated text.** A secret of `1` replaces every `1` in
  the output, including digits of a status code or a duration. It also rewrites
  numbers in JSON: a timestamp such as `created_at` in `meta.json` becomes a
  string with the digits replaced. Use a value that is long enough to be
  distinctive.
- **Only values `da` read are secrets.** A program a case starts inherits the
  whole environment. If it prints a variable that no `${env:NAME}` reference
  read, `da` never saw that value and cannot redact it.
- **Other transformations are not recognized.** Encodings other than percent
  encoding and base64, a second round of either encoding, compression, hashes,
  and values a program splits, reorders or reassembles still appear as they are.
- **base64 wrapped at a fixed width is not recognized.** A value that was
  encoded and then broken across lines at a fixed width, the way MIME and PEM
  wrap, no longer matches after the first line break, because the newline
  splits the encoded characters.
- **A JSON scalar changes type when it contains a secret.** A number, a boolean
  or `null` that is redacted becomes a string, so the type of that one value
  changes in the report.
- **Only the value is secret.** The name of the variable and the fact that it is
  set are not.

### Schema

`da schema` prints a [JSON Schema](https://json-schema.org/) (draft 2020-12) for
one case file. The schema follows the parser: only `name`, `project`, `suite`,
`layer`, `tags`, `vars`, `observe`, `steps` and `finally` at the top level; one registered
step name per step, plus optional `optional`, `retry`, `expect` and `save`, where a step
that saves may not be optional; one registered observer
and its arguments per entry of `observe`; the four comparisons with their value
types; and each step's and observer's argument schema under `$defs`, together
with its description. The classification fields are optional strings that cannot
be empty, `tags` is an array of unique non-empty strings, and an observation name
matches `^[a-z0-9._-]+$`. The registry builds it, so a build with fewer features
describes fewer steps and observers. `--output text` (the default) pretty-prints
the schema and `--output json` prints it compactly; both are the same document,
and the command exits 0.

Save the output as a file next to the cases and point an editor at it. For
[yaml-language-server](https://github.com/redhat-developer/yaml-language-server),
add a modeline to each case file:

```yaml
# yaml-language-server: $schema=./da-case.schema.json
name: Login returns a token
steps:
  - http.request:
      url: http://127.0.0.1:8080/health
```

Or point the server's `yaml.schemas` setting at the file once:

```json
{
  "yaml.schemas": {
    "./da-case.schema.json": "cases/**/*.yaml"
  }
}
```

The schema describes the shape of a case, not every check: undefined variables
and the syntax of JSON paths still need `da lint`. A bound that a step declares
for an argument, such as a timeout of at least 1, is in the schema as well: it
appears as `minimum` in the step's `$defs` entry, and `da lint` enforces the
same bound. A build with `--no-default-features` still prints a valid schema,
but no step is registered, so the step item accepts nothing.

## Case files

A case is a YAML file with a name, optional variables, optional observations and
a list of steps. Each step is a mapping with exactly one step name, optionally
followed by `optional`, `retry`, `expect` and `save`:

```yaml
name: Login returns a token
vars:
  port: "8080"
steps:
  - http.request:
      method: POST
      url: http://127.0.0.1:${port}/login
      json: { user: alice, password: secret }
    expect:
      - path: $.status
        equals: 200
      - path: $.body.token
        exists: true
    save:
      token: $.body.token
  - http.request:
      url: http://127.0.0.1:${port}/me
      headers: { Authorization: "Bearer ${token}" }
    expect:
      - path: $.body.name
        equals: alice
```

Unknown fields in a case file, unregistered step names, unregistered observers,
step or observer arguments that do not match their schema, and undefined
variables are all case errors. A case may also declare `project`, `suite`,
`layer` and `tags` (see
[Classification and filtering](#classification-and-filtering)) and `observe`
(see [Observers](#observers)).

### Variables

`vars:` defines variables for the whole case. `save:` copies a value out of a
step result into a variable. A later step argument can use any defined variable
as `${name}`. Substitution is textual and only happens inside strings, so a
variable may not change the type of a value. Referencing a variable that is
neither in `vars:` nor saved by an earlier step is a case error. `--var
name=value` on the command line defines a variable for every case and overrides
the same name in `vars:`; `--vars-file` supplies a whole set of them (see
[Usage](#usage)).

`${env:NAME}` reads the process environment instead of a variable. It works in
a step argument, in a `vars:` value and in a `--vars-file` value, so a
connection string or a token can stay out of the case file:

```yaml
vars:
  base: "${env:DA_BASE_URL}"
steps:
  - http.request:
      url: "${base}/health"
      headers: { Authorization: "Bearer ${env:DA_TOKEN}" }
    expect:
      - path: $.status
        equals: 200
```

- The name must start with a letter or `_` and continue with letters, digits or
  `_`. An empty name or any other character is a case error (exit 3).
- A name that is not set in the environment is a case error (exit 3) whose
  message names it and says to set it before running. `da lint` reports the same
  error, so a run that would fail this way never starts.
- A name set to an empty string counts as set and expands to nothing.
- There is no default-value syntax such as `${env:NAME:-fallback}`; a missing
  value is always an error, so a typo cannot silently use a fallback.
- Only `${env:NAME}` is expanded inside a variable's value. A `${name}`
  reference there stays as written, even where the variable is later used in a
  step argument: substitution is a single pass and does not rescan the text it
  inserts.

A value read from the environment is a **secret**, and `da` replaces it with
`***` in everything it writes. See [Secrets](#secrets).

### Assertions

`expect:` is a list of assertions evaluated against the step's JSON result. Each
entry has a `path` — a [JSONPath](https://www.rfc-editor.org/rfc/rfc9535.html)
expression such as `$.body.items[0].id` — and exactly one comparison:

| Comparison | Meaning |
| --- | --- |
| `equals` | The selected value equals the expected JSON value. |
| `not_equals` | The selected value differs from the expected JSON value. |
| `contains` | A string contains a substring, or an array contains a value. |
| `exists` | `true` requires a value, `false` requires none. |

Except for `exists`, the path must select exactly one value; a path that matches
nothing fails the assertion and is reported as such instead of being compared
with `null`. Member names that are not plain identifiers need bracket notation,
for example `$.body.headers['content-type']`.

### Optional steps

`optional: true` marks a step whose work is allowed to be absent: a banner that
only sometimes appears, a probe for a feature that may not be deployed, or a
cleanup step for data an earlier step may not have created.

```yaml
name: A banner that only sometimes appears
steps:
  - http.request:
      url: http://127.0.0.1:8080/banner
    optional: true
    expect:
      - path: $.status
        equals: 200
  - http.request:
      url: http://127.0.0.1:8080/health
    expect:
      - path: $.status
        equals: 200
```

An optional step is reported as `skipped`, and the case continues with the next
step, when it did not reach its assertions:

- an assertion did not pass;
- the step reported an execution error, meaning it ran but could not finish its
  work;
- the step exceeded its timeout.

The step keeps its `result`, its `assertions` and the `error` that skipped it, so
a reader can still see why the work was absent; `--output json` carries all
three, and text output prints the problem with an `(optional)` note. A case whose
only failures are skipped optional steps passes, and the summary counts those
steps under `skipped`.

An environment error or an invalid-input error is **not** skipped: it is reported
as `error` and ends the case, with the exit code that error always has (4 for an
environment error, 3 for invalid input). Those errors say the environment is
broken — an unreachable target, a browser that died, a log file that cannot be
read — and a broken environment must never look like a feature that is simply
absent. In the example above a 404 fails an assertion and is skipped, while a
refused connection is an environment error and stops the case.

An optional step may not also declare `save`. A skipped step has no result to
copy, so a later `${name}` would be undefined; a case that combines the two is
refused while it is parsed, with the location of `save`, by `da lint` and by
`da run` before any case runs. Remove `save`, or drop `optional` so the step
always runs. `optional: false` is allowed and changes nothing.

`optional` takes a literal `true` or `false`; a quoted string such as
`optional: "true"` is a case error. The field name is reserved, so no step can be
registered as `optional`.

### Retrying read-only steps

`retry` polls a read-only step until every assertion in `until` passes or the
time budget expires. Use it for asynchronous work that becomes observable
later — a background job finishing, a row appearing in the database, or text
updating on a page — without repeating side effects.

```yaml
name: Wait for a background job to finish
steps:
  - http.request:
      method: POST
      url: ${base}/jobs
      json: { task: export }
    save:
      job_id: $.body.id
  - http.request:
      url: ${base}/jobs/${job_id}
    retry:
      until:
        - path: $.status
          equals: 200
        - path: $.body.state
          equals: ready
      timeout_ms: 5000
      interval_ms: 200
    expect:
      - path: $.body.count
        equals: 10
    save:
      result_url: $.body.url
```

| Field | Meaning |
| --- | --- |
| `until` | Non-empty list of assertions using the same syntax as `expect`. Required. |
| `timeout_ms` | Total polling budget in milliseconds (at least `1`). Required. |
| `interval_ms` | Wait between failed attempts in milliseconds (at least `1`). Defaults to `250`. |

Only read-only steps may declare `retry`:

- `http.request` with exact uppercase `method` `GET` or `HEAD` (including when `method` is omitted, which defaults to `GET`)
- `db.query`
- `browser.read`
- `browser.read_clipboard`
- `browser.wait`

State-changing steps — `http.request` with `POST`, `PUT`, `PATCH` or `DELETE`
(or an unresolved variable method), `cli.run`, `browser.open`,
`browser.click`, `browser.type`, `browser.upload`, `browser.press`,
`browser.scroll`, `browser.hover`, `browser.drag`, `browser.set_clipboard`,
`browser.set_cookies`, `browser.save_state`, `browser.screenshot` and
`browser.switch_tab` — are refused statically by `da lint` and before any step
runs in `da run` (exit 3). `retry` is a reserved step field name, so no step may
register under that name.

Each polling iteration runs the step and any declared `observe` observers for
that attempt, keeping only the last attempt's result and observations:

- `timeout_ms` is the polling budget for **starting** a new attempt; it never
  cancels or interrupts an attempt that is already in flight (each individual
  call keeps the step's own `timeout_ms`). If an already in-flight attempt
  finishes after `retry.timeout_ms`, its completed result or error is still
  evaluated normally, and no further attempt is started.
- When every assertion in `until` passes, polling stops immediately; `expect`
  and `save` run against that result, and both `until` and `expect` assertions
  appear in the step report.
- When `until` fails, or the step returns an execution error or timeout, `da`
  waits `interval_ms` before starting another attempt only while enough time
  remains strictly within `timeout_ms`. If `interval_ms` is longer than or equal
  to the remaining budget, or if the wait exhausts the remaining budget, `da`
  does not start a new attempt and reports the last completed failed result or
  last error (or `skipped` when combined with `optional: true`).
- An environment error (exit 4) or invalid-arguments error (exit 3) aborts
  polling immediately instead of waiting for `timeout_ms`.
- `retry` is a case-file feature. `da session step` rejects a step that carries
  `retry` with an invalid-input error instead of running or ignoring the policy;
  an interactive caller sends each attempt explicitly.

### Cleanup with `finally`

`finally:` is an optional, non-empty list of cleanup steps at the top level of
a case. It uses the exact same step syntax, argument schemas, `optional`,
`retry`, `expect` and `save` rules as `steps:`.

```yaml
name: Create and always remove an order
steps:
  - http.request:
      method: POST
      url: http://127.0.0.1:8080/orders
      json: { item: book }
    expect:
      - path: $.status
        equals: 201
    save:
      order_id: $.body.id
  - http.request:
      url: http://127.0.0.1:8080/orders/${order_id}
    expect:
      - path: $.body.state
        equals: confirmed
finally:
  - http.request:
      method: DELETE
      url: http://127.0.0.1:8080/orders/${order_id}
    optional: true
```

Execution and evidence guarantees:

- **Always runs before sessions close.** Every step in `finally:` runs in
  declaration order after `steps:` finishes — whether `steps:` passed, failed
  an assertion, or stopped on an error — and before sessions close. A failing
  or errored `finally:` step never skips the remaining `finally:` steps.
- **Failure evidence is locked in before cleanup.** When `steps:` did not pass
  and failure bundles (or `--record`) are enabled, `da` collects session
  evidence (such as browser screenshots and DOM snapshots) **before** entering
  `finally:`. Cleanup actions that navigate away or delete records therefore
  cannot overwrite the failure scene.
- **Cleanup failures still fail the case.** When `steps:` passed and one or
  more `finally:` steps fail or error, `da` collects session evidence after
  `finally:` finishes (before closing sessions) and gives the case the first
  failing `finally:` step's problem. When `steps:` already failed, the case
  keeps the main failure as `error` and records each `finally:` outcome under
  `finally`.
- **Variables and `optional: true`.** Static checks (`da lint` and preflight)
  allow `finally:` steps to reference `vars:` as well as variables saved in
  `steps:` or earlier `finally:` steps. If an earlier step failed before
  writing a `save` variable, referencing `${name}` in `finally:` fails as an
  execution error — or add `optional: true` to that cleanup step so it is
  cleanly `skipped` while remaining `finally:` steps continue.

### `http.request`

| Argument | Meaning |
| --- | --- |
| `method` | HTTP method. Defaults to `GET`. |
| `url` | Absolute URL including the scheme. Required. |
| `headers` | Request headers. |
| `query` | Query parameters appended to the URL, percent-encoded. |
| `json` | JSON request body; `content-type: application/json` is added unless a header sets it. |
| `body` | Raw request body. Mutually exclusive with `json`. |
| `timeout_ms` | Timeout for the whole request. Defaults to `30000`. |

The result is an object with `status` (a number), `headers` (lower-cased names)
and `body`. A body that is valid JSON becomes a JSON value; anything else stays a
string. A non-2xx status is not an error: it is part of the result and is judged
by assertions. A refused connection or an unresolvable host is an environment
error; exceeding `timeout_ms` is a timeout.

### `cli.run`

Runs a program without a shell and returns how it ended and what it wrote.

| Argument | Meaning |
| --- | --- |
| `command` | Program name or path. Required. A relative path is resolved against the working directory of `da`. |
| `args` | List of arguments, one per entry. Nothing is quoted, expanded or split. |
| `cwd` | Working directory of the program. Defaults to the working directory of `da`. |
| `env` | Variables added to the inherited environment, replacing names that are already set. |
| `stdin` | Text written to the program's standard input. Without it the input is empty. |
| `timeout_ms` | Timeout for the whole run. Defaults to `30000`. |

The result is an object with `exit_code`, `stdout` and `stderr`, plus `json` when
standard output is JSON:

```yaml
steps:
  - cli.run:
      command: git
      args: ["rev-parse", "--short", "HEAD"]
    expect:
      - path: $.exit_code
        equals: 0
      - path: $.stderr
        equals: ""
      - path: $.json
        exists: false
```

A non-zero exit code is not an error: it is part of the result and is judged by
assertions. `exit_code` is `null` when the program was killed by a signal.
Standard output and standard error are decoded as UTF-8, lossily. `json` is only
present when standard output without surrounding whitespace parses as JSON, so a
case can test it with `exists`.

The arguments are passed to `std::process::Command` directly, so shell syntax
such as quotes, `|` or `&&` does not apply. Run a shell explicitly when a case
needs one, for example `command: sh, args: ["-c", "..."]`. A relative `command`
and `cwd` are resolved against the working directory of the `da` process, not
against the case file.

A program that cannot be started is an environment error (exit 4) whose message
names it and says what to check. When `timeout_ms` expires the program is killed
and the step is a timeout (exit 5). Both pipes are read on their own threads, so
a program that writes more than a pipe buffer cannot deadlock the run; after the
program ends `da` waits at most one second for the pipes to close, so a process
the program left behind cannot hang it.

### `db.query`

Reads the database to check what an earlier step wrote. The step runs exactly
one read-only statement against SQLite or PostgreSQL and returns what the
database answered.

| Argument | Meaning |
| --- | --- |
| `url` | Connection string. Required. `sqlite:<path>` names a SQLite file, `postgres://` and `postgresql://` are a PostgreSQL URL. |
| `sql` | One SQL statement. Required. |
| `params` | Values bound to the placeholders, in order. Each one is a string, a number, a boolean or null. |
| `max_rows` | Largest number of rows to return. Defaults to `1000`; the step reads one row more to know whether the result was longer. |
| `timeout_ms` | Timeout for connecting and for the whole query. Defaults to `30000`. |

```yaml
name: The order was written
steps:
  - http.request:
      method: POST
      url: ${base}/orders
      json: { id: 42, total: 19.9 }
    save:
      order_id: $.body.id
  - db.query:
      url: ${env:DATABASE_URL}
      sql: SELECT status, total FROM orders WHERE id = $1
      params: ["${order_id}"]
    expect:
      - path: $.rows[0].status
        equals: paid
      - path: $.row_count
        equals: 1
```

The result is an object with `columns`, `rows`, `row_count` and `truncated`:

| Field | Meaning |
| --- | --- |
| `columns` | Column names, in the order the database returned them. |
| `rows` | One object per row, keyed by column name. |
| `row_count` | Number of entries in `rows`. |
| `truncated` | `true` when the statement matched more rows than `max_rows`, in which case only `max_rows` are returned. |

A SQLite `url` is a path: a relative one is resolved against the working
directory of `da`, not against the case file. `sqlite:` also accepts an absolute
path, for example `sqlite:C:/data/app.db` or `sqlite:/var/lib/app.db`. Any other
prefix is an invalid argument (exit 3).

#### Parameters

SQLite takes `?` for an anonymous placeholder and `?1`, `?2` for numbered ones;
PostgreSQL takes `$1`, `$2`. A boolean bound to SQLite becomes `1` or `0`,
because SQLite has no boolean type of its own. A parameter list that does not
match the placeholders is refused by the database.

On PostgreSQL each value is sent at the width its placeholder declares, so
`WHERE id = $1` works against an `integer` column with `params: [42]`. A number
that does not fit the placeholder — `70000` for a `smallint`, a fraction for an
`integer` — is an invalid argument (exit 3) whose message names the parameter
and the type.

#### Types

| Database type | JSON |
| --- | --- |
| integer | number |
| real, double precision | number |
| text, varchar, char, enum | string |
| boolean (PostgreSQL) | `true` / `false` |
| `NULL` | `null` |
| blob, bytea | lower-case hexadecimal string, for example `"00ff"` |
| `numeric`, `decimal`, `uuid`, `date`, `time`, `timestamp`, `timestamptz`, `interval`, arrays, ranges, and any other PostgreSQL type | string, exactly as PostgreSQL prints it under `::text` |
| `json`, `jsonb` | the JSON value itself |

A PostgreSQL column whose type is not in the first rows of that table is
requested as `::text` in a wrapper query, so it still arrives as a string
instead of failing the step. The column names, the order and the `params` are
unchanged by that wrapper.

#### Read-only

`db.query` cannot write, and the guarantee does not rest on looking at the SQL:

- SQLite opens the file read-only, so the connection has no write permission,
  and it also sets `PRAGMA query_only`. A database file that does not exist is
  an environment error (exit 4) — the step never creates one.
- SQLite also registers an authorizer that allows only the actions a read
  takes: `SELECT`, reading a column, calling a function, a recursive common
  table expression, and a fixed list of read-only `PRAGMA` names such as
  `table_info`, `index_list` and `integrity_check`. Every other action — a
  write, a schema change, `ATTACH`, `DETACH`, a transaction — is denied while
  the statement is prepared, so nothing in the statement runs.
- A statement that SQLite itself reports as one that would write is refused
  right after it is prepared, before its first step. `VACUUM` is refused there:
  SQLite asks the authorizer nothing about it, and `VACUUM INTO 'copy.db'`
  creates the target file when it runs.
- PostgreSQL sets `SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` and runs
  the statement in an explicit `READ ONLY` transaction that is always rolled
  back. A statement that tries to write is refused by the server.
- Only one statement is accepted. SQLite refuses a second statement after it
  prepares the first one, and PostgreSQL prepares through the extended protocol,
  which cannot carry two commands.

`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`, a write hidden in a common table
expression such as `WITH gone AS (DELETE FROM orders RETURNING *) SELECT * FROM
gone`, and a statement list such as `SELECT 1; DELETE FROM orders` all fail the
step with an environment error (exit 4) whose message says the step is read-only.
The database is left exactly as it was. SQLite does not accept a data-modifying
common table expression at all, so there the statement is a syntax error before
any read-only layer sees it; PostgreSQL accepts the syntax, and the read-only
transaction is what refuses it.

The refusal is also a promise about the file system: a refused `VACUUM INTO
'copy.db'` or `ATTACH DATABASE 'other.db' AS other` leaves no file at that path.
No data is copied, and the target is never opened.

#### Connection strings and secrets

Never write a password into a case file. Read the whole connection string from
the environment instead:

```yaml
- db.query:
    url: ${env:DATABASE_URL}
    sql: SELECT 1
```

A value that comes from `${env:NAME}` is a secret: it is replaced with `***` in
the terminal, in `--output json` and in every file of a failure bundle, so a
connection string that appears in an error message cannot leak the password. See
[Secrets](#secrets).

A refused connection and a statement the server rejects are both environment
errors (exit 4); exceeding `timeout_ms` is a timeout (exit 5). The message names
the SQLite file it could not open, but never repeats a PostgreSQL URL, which
usually carries a password.

### Browser steps

The `browser.*` steps drive one page in a Chrome or Chromium that is already
installed. They are provided by the `browser` feature and never download a
browser. A case has one page: the first `browser.open` creates it, every later
step of the same case uses it, and a second `browser.open` navigates that same
page instead of opening another one. A page action such as a link with
`target="_blank"` or a `window.open` call opens another tab; the session keeps
driving the tab it was on until [`browser.switch_tab`](#tabs-opened-by-a-page-action)
selects the new one.

By default the case starts its own browser. With `connect`, it instead drives a
browser the user already runs with a debug port, so the user's logged-in state is
available. The case opens a new tab there and closes only the tabs it opened —
that tab and the tabs its pages opened — when it ends; the user's browser keeps
running and the user's other tabs are untouched.

| Step | Arguments | Result |
| --- | --- | --- |
| `browser.open` | `url` required; optional `connect`, `profile`, `storage_state`, `testid_attribute` (defaults to `data-testid`), `dialog`; `headless` defaults to `true`; `width` and `height` default to 1280×800; `timeout_ms` defaults to 30000 | `url`, `title`; `dialog` when one opened |
| `browser.click` | locator, or explicit `at` coordinates; optional `dialog` | `{}`, or the clicked element's `text` when it has one; with `at`, the clicked `x`, `y` and `viewport`; `dialog` when one opened |
| `browser.type` | locator, or explicit `at` coordinates; `text` required unless `value` is given; optional `dialog` | `{}`; with `at`, the clicked `x`, `y` and `viewport`; `dialog` when one opened |
| `browser.upload` | locator; `files` required (a list of local paths; `[]` clears the selection); `timeout_ms` defaults to 5000 | `files`, `names` |
| `browser.press` | `key` required, for example `Enter` or `Tab`; optional `dialog` | `{}`; `dialog` when one opened |
| `browser.wait` | locator, or `url_contains`; `timeout_ms` defaults to 5000; optional `dialog` | `url`; `dialog` when one opened |
| `browser.read` | locator; optional `attribute` | `text`; with `attribute`, also `value` (`null` when the attribute is absent) |
| `browser.scroll` | viewport `delta_x`/`delta_y`, `at` coordinates + `delta_x`/`delta_y`, locator + `delta_x`/`delta_y`, or locator (with optional `into_view: true`); optional `timeout_ms`, `dialog` | `scrolled` (`true`), `scroll_x`, `scroll_y`; `into_view`, `scroll_left`/`scroll_top`, or `x`/`y`/`viewport` when applicable; `dialog` when one opened |
| `browser.hover` | locator, or explicit `at` coordinates; optional `timeout_ms`, `dialog` | `hovered` (`true`), optional `text`; with `at`, `x`, `y` and `viewport`; `dialog` when one opened |
| `browser.drag` | `from` and `to` (each a locator, or explicit `at` coordinates); optional `timeout_ms`, `dialog` | `dragged` (`true`), `from: { x, y }`, `to: { x, y }`; `viewport` when an endpoint uses `at`; `dialog` when one opened |
| `browser.set_clipboard` | `text` required (supports `${env:NAME}` secrets); optional `timeout_ms` | `characters` (character count only; never echoes `text`) |
| `browser.read_clipboard` | optional `timeout_ms`; safe with `retry` | `text` (redacted with `***` when matching `${env:NAME}`) |
| `browser.screenshot` | `path` required | `path`, `bytes` |
| `browser.set_cookies` | `cookies` (non-empty array of `{ name, value, url \| domain, path?, http_only?, secure?, same_site? }`) and/or `local_storage` (non-empty `<origin> \| current` → `{ key: value }` map); at least one of the two | `cookies_set`, `local_storage_set` |
| `browser.save_state` | `path` required | `path`, `cookies`, `local_storage` (counts only) |
| `browser.switch_tab` | `url_contains`, `title_contains` or `index` (optional; defaults to the tab a page action opened most recently); `timeout_ms` defaults to 5000 | `url`, `title` |

A **locator** takes **exactly one** of the five ways below, plus any modifiers:

| Locator | Meaning |
| --- | --- |
| `css: <selector>` | Matches elements with a CSS selector, including elements inside any number of nested open shadow roots (composed pre-order). |
| `xpath: <expression>` | Matches elements with an XPath expression (`document.evaluate`). XPath cannot enter a shadow root; see [Shadow DOM](#shadow-dom). |
| `text: <text>` | Matches visible text after collapsing runs of whitespace to one space and trimming ends; only the innermost matching visible element counts, including elements inside open shadow roots. |
| `role: <role>` (optional `name: <name>`) | Matches non-ignored nodes in Chrome's accessibility tree (`Accessibility.queryAXTree`) with that role and exact accessible name, including nodes inside shadow roots. |
| `testid: <value>` | Matches elements whose test-id attribute equals `<value>`, including elements inside any number of nested open shadow roots. Defaults to `data-testid`; override per session with `testid_attribute` on `browser.open`. |

| Modifier | Meaning |
| --- | --- |
| `within: <locator>` | Narrows the search to elements inside the unique element matched by this container locator; may be nested. When `within` matches a same-origin `<iframe>` or `<frame>`, the inner locator searches that frame's document (and may nest further). When it matches a shadow host, the inner locator searches that host's open shadow root. |
| `nth: <integer>` | Selects the zero-based match (`0`, `1`, …) in document order, composed pre-order when open shadow roots are involved. Explicit `nth` permits multiple matches and waits until that index exists (failing on timeout if still out of range). |
| `timeout_ms: <ms>` | How long the step polls before timing out (defaults to 5000). |

Giving zero or more than one locator key, passing `name` without `role`, passing
a negative or non-integer `nth`, giving an empty `testid_attribute`, or giving
an invalid `within` locator is an argument error reported by `da lint` before
anything runs. `browser.wait` keeps its `url_contains` form as an alternative to
an element locator. `browser.click`, `browser.type`, `browser.scroll`,
`browser.hover` and `browser.drag` endpoints take `at` instead of a locator; see
[Coordinates](#coordinates). Inside [`da session`](#interactive-sessions-da-session),
interactive steps also accept `ref: e<N>` targets from the latest snapshot,
which `da session export` converts to ordinary stable locators.

#### Choosing a locator

Prefer `testid` and `role` + `name` over `css` or `xpath`:

- Use `testid` when you control the markup: `data-testid` (or a custom
  `testid_attribute` set on `browser.open`) survives layout and copy changes.
- Use `role` + `name` when you want to assert the control's accessible contract
  (for example `role: button` with `name: Submit`, or `role: textbox` with
  `name: Email`), because implicit roles and `<label for="...">` associations work
  without extra attributes.
- Use `text` for static non-interactive copy, and keep `css` or `xpath` for
  structure that has no test id or accessible role, paired with `within` instead
  of fragile positional chains.

#### Coordinates

Some content has no DOM element and no accessibility node to name: a
`<canvas>` painting, a WebGL scene, or a map control. For that content only,
`browser.click`, `browser.type`, `browser.scroll`, `browser.hover` and
`browser.drag` endpoints (`from` / `to`) accept explicit viewport coordinates
instead of a locator:

```yaml
- browser.click:
    at:
      x: 100
      y: 50
      viewport: { width: 1280, height: 800 }
```

| Argument | Meaning |
| --- | --- |
| `at.x`, `at.y` | Position in CSS pixels from the top-left corner of the page viewport. A whole or fractional number that is not negative. |
| `at.viewport.width`, `at.viewport.height` | The viewport the coordinates were measured at. Optional, but recording it is what makes the step replayable. |

`at` is a locator of its own: it cannot be combined with `css`, `xpath`, `text`,
`role`, `testid`, `within`, `nth` or `name`, and a step that gives both is refused
before it runs. `expect` and `save` work on a coordinate step exactly as on any
other step.

Before the click, the step reads the page's live CSS viewport. When `viewport` is
recorded and differs from the live size, the step fails with the recorded and the
live size in the message instead of clicking the wrong place; a different screen
resolution can therefore never silently hit another target. A coordinate that is
not smaller than the live viewport's width or height fails the same way, and a
negative coordinate is refused as an argument error before the browser is
touched. Then the step dispatches the mouse move, press and release events and
returns where it clicked and at what size:

```json
{ "x": 100, "y": 50, "viewport": { "width": 1280, "height": 800 } }
```

A bare `at: { x, y }` without `viewport` is allowed. It clicks at whatever size
the page currently has, still refuses a coordinate outside that viewport, and
reports the viewport it used, so a caller can record it. `browser.type` clicks
the coordinates first to focus the target and then types `text` (or `value`); it
does not accept both at once with `at`.

Coordinates are a last resort. A layout change, a different window size, a
retina scale or a translated page moves the target without changing the case, so
`da lint` reports every coordinate step and still exits 0. The warning goes to
standard error, one line per warning, and the standard-output summary counts it:

```text
$ da lint cases/editor.yaml
cases/editor.yaml: warning: line 6 column 7: step 2: `browser.click` uses explicit coordinates `at`; coordinates are a last resort for content that has no DOM element or accessibility node (for example `<canvas>`) and they click the wrong place when the layout or viewport changes, so prefer `css`, `xpath`, `text`, `role` or `testid` when one can name the target
Checked 1 file: 0 errors, 1 warning.
```

A coordinate step also warns when it records no `at.viewport`, because then a
different size is not detected. `da session export` therefore writes the
viewport it observed into every exported coordinate step and lists the step in
the export report's `fragile` array, so the exported case is replayable at the
same size. The YAML key `y` is written unquoted and read as written: `da` keeps
mapping keys as text instead of applying YAML 1.1 boolean inference, so
`y: 50` is the vertical coordinate, not `true`.

#### Coordinates from an outside model

`da` has no model and never interprets the pixels of a page. A caller that
already has one — its own model or tool — keeps it outside and passes the answer
in through a coordinate step. The loop has three parts, and only the first and
the last are steps of the case:

1. `browser.screenshot` writes a PNG of the page to `path`, resolved against the
   directory that holds the case file, and returns the `path` and the number of
   `bytes` written.
2. The caller's own model or tool reads that PNG and computes a point in it.
3. `browser.click`, `browser.type`, `browser.scroll`, `browser.hover` or
   `browser.drag` acts on that point given as `at: { x, y, viewport }`, with
   `viewport` recording the size the point was measured at. A coordinate
   `browser.type` clicks the point to focus it and then types `text`.

Record the viewport with the point. Before acting, the step reads the page's live
CSS viewport and, when the recorded and the live size differ, fails with both
sizes in the message instead of clicking whatever now sits at that position.
`at: { x, y }` without `viewport` still clicks, but then no size is compared and
`da lint` warns about that as well (see [Coordinates](#coordinates)).

```yaml
name: Mark the canvas point
steps:
  - browser.open:
      url: http://127.0.0.1:8080/editor
  # 1. capture the page; the PNG lands beside the case file
  - browser.screenshot:
      path: canvas.png
  # 2. an outside model or tool reads canvas.png and returns 640, 400
  # 3. click that point at the size it was measured at
  - browser.click:
      at:
        x: 640
        y: 400
        viewport: { width: 1280, height: 800 }
```

The point is in CSS pixels, the unit `at.x` and `at.y` take. `da` stops at the
coordinate: it never parses the image it wrote, never checks that a guessed point
landed on a target, and never retries or adjusts it. A coordinate step acts once,
at the numbers it was given; the argument rules and the recorded-versus-live
viewport check above are the only guards. Whether the click hit what the image
showed is the caller's to assert — with `expect` on the step result, a
`browser.read`, or another `browser.screenshot` sent back to the model. Because
the case records the point and not the model, replaying it at the same size
repeats the same click.

`da lint` accepts the case above and reports the coordinate warning quoted under
[Coordinates](#coordinates), at the line and column of the step's arguments,
with `Checked 1 file: 0 errors, 1 warning.` on standard output and exit 0. The
warning stays when `viewport` is recorded: the recorded size turns a different
window into a failure instead of a wrong click, but it does not make the point
follow a layout change, a translated page or a zoom, so a reader of the case
still has to know that the step is pinned to one window and one layout. It goes
away only when the target can be named with `css`, `xpath`, `text`, `role` or
`testid` instead.

#### Strict unique matching

Without `nth`, every locator must match **exactly one** candidate element —
the first match is never silently picked:

- For `css`, `xpath` and `testid`, all matching elements are collected regardless
  of visibility; for `text` and `role`, #48 candidate filtering applies
  (innermost visible text matches; visible non-ignored AX nodes).
- If `0` candidates match, the step keeps polling until an element appears (and,
  when required, becomes visible) or `timeout_ms` expires.
- If **more than 1** candidate matches, the step **fails immediately** without
  waiting, reporting the locator, total match count, and up to 5 candidates
  (tag name, `id`, and text truncated to 60 characters, redacted with `***` when
  a secret appears). For example:

```text
CSS selector `.item` matched 7 elements; narrow the locator or pass `nth` (candidates: 1: <li#item-1> "First item", 2: <li#item-2> "Second item ...", 3: <li#item-3> "Token ***", 4: <li#item-4> "Item 4", 5: <li#item-5> "Item 5" (showing 5 of 7))
```

When multiple matches are expected (such as rows in a list), narrow the search
with `within` or pick an explicit zero-based `nth`.

#### Actionability checks

Before clicking, typing, hovering or dragging, `browser.click`, `browser.type`,
`browser.hover` and locator endpoints of `browser.drag` wait within
`timeout_ms` for all five checks in order:

1. **Exists**: unique match (or valid `nth` index).
2. **Visible**: `display` is not `none`, `visibility` is not `hidden` or
   `collapse`, and `width`/`height` are positive.
3. **Enabled**: neither the `disabled` attribute (including disabled `<fieldset>`)
   nor `aria-disabled="true"` applies.
4. **Stable**: viewport position and dimensions are unchanged across two
   consecutive polls (`scrollIntoView` brings offscreen elements in first).
5. **Unoccluded**: `document.elementFromPoint` at the element's center returns
   the target element or one of its descendants. Each open shadow root at the
   point is descended in turn, so a target inside a shadow root is compared with
   the element that really receives the click rather than with its host (see
   [Shadow DOM](#shadow-dom)).

If `timeout_ms` expires, the error names the check that failed — and when
occluded, identifies the blocking element (tag, `id`, and snippet). `browser.read`
and `browser.wait` only wait for existence and visibility. `browser.upload` waits
only for existence and an enabled input: a hidden file input is exactly what real
pages use, so it never waits for the control to be painted.

#### Frames (`iframe` and `frame`)

Unscoped locators search only the main document. To interact with elements inside
a same-origin `<iframe>` or `<frame>`, point `within` at the frame element;
`within` can be nested across any combination of containers and inner frames:

```yaml
- browser.click:
    role: button
    name: Save card
    within:
      css: "#nested-frame"
      within:
        css: "#embed"
```

When `within` resolves to a same-origin frame, `da` enters its `contentDocument`
(waiting until it finishes loading within `timeout_ms`), converts element
coordinates across every enclosing frame's viewport offset, border, padding, and
axis-aligned CSS scaling (`transform: scale(...)` / `translate(...)`, individual
`scale`/`translate`, `zoom`, and scaled ancestor wrappers), and checks
`elementFromPoint` in each enclosing document as well as the inner frame — so
clicks land at the true viewport coordinates and a top-level overlay covering an
iframe element still fails as occluded. If an enclosing frame has a CSS transform
that cannot be mapped to axis-aligned viewport coordinates (such as rotation,
skew, reflection, or a 3D transform), actionability checks fail immediately as
an execution error (exit 1) instead of clicking the wrong coordinates.

Cross-origin frames (including `<iframe sandbox>` without `allow-same-origin`)
fail immediately as an execution error (exit 1) without waiting out
`timeout_ms`, because the browser blocks access to their `contentDocument`.

Inside [`da session`](#interactive-sessions-da-session), `snapshot` and `ref`
targets stay scoped to the main document (showing frames as `Iframe` nodes),
while explicit `within` locators work in `da session step` and export verbatim.

#### Tabs opened by a page action

Clicking a link with `target="_blank"`, or a control whose handler calls
`window.open`, opens a second tab. The session stays on the tab it was driving —
an advertisement, a download page or a sign-in popup therefore cannot move the
case by itself — and `browser.switch_tab` selects the new tab explicitly:

```yaml
name: Follow a link that opens a new tab
steps:
  - browser.open:
      url: http://127.0.0.1:8080/orders
  - browser.click:
      css: "#new-order"
  - browser.switch_tab:
      url_contains: "/orders/new"
    expect:
      - path: $.title
        equals: New order
  - browser.type:
      css: "#customer"
      text: Ada
  - browser.screenshot:
      path: new-order.png
```

`browser.switch_tab` takes:

| Argument | Meaning |
| --- | --- |
| `url_contains: <text>` | Keep the tabs whose URL contains `<text>`. |
| `title_contains: <text>` | Keep the tabs whose title contains `<text>`. |
| `index: <integer>` | Select the tab at this zero-based position in the order the case created its tabs (`0` is the tab `browser.open` opened). |
| `timeout_ms: <ms>` | How long to wait for a matching tab (defaults to 5000). |

The filters combine (a tab must satisfy all of them), and, like a locator, a
filtered switch must match **exactly one** tab: a second match fails immediately
as an execution error whose message lists the candidates with their positions.
Without any filter the step selects the tab a page action opened **most
recently**; it never falls back to the tab the case already drives, so a case
that opened nothing keeps waiting and then fails. A tab that is not there yet is
waited for: the step polls until `timeout_ms` expires and then fails with the
configured filters and every tab the case owns, for example:

```text
no tab matched a URL containing `/browser/probe/absent` within 400 ms (tabs: 0: "http://127.0.0.1:8080/orders" "Orders")
```

After the switch, every later `browser.*` step drives the new tab: `read`,
`click`, `type`, `press`, `scroll`, `hover`, `drag`, `set_clipboard`,
`read_clipboard`, `wait`, `screenshot`, `browser.open`, and the network
requests `browser.network` reports. The failure bundle (`screenshot.png` and
`dom.html`) is collected from the tab the case was on, because that is the tab
whose failure the case is reporting. The previous tab stays open, so a second
`browser.switch_tab` can select it again, by `title_contains`, `url_contains` or
`index`.

**Which tabs a case can select.** A case owns exactly two kinds of tabs: the tab
`browser.open` created for it, and every tab a page of this case opened (Chrome
reports those with an `openerId` inside the set). Only those are enumerated,
switched to and closed. With `connect`, that rule is what keeps the user's
browser safe: a tab the user already had open was not opened by the case, so it
is never adopted, never selected by `browser.switch_tab`, and never closed —
even when it matches `url_contains` or `title_contains`. A tab of another browser
context is not adopted either.

When the case ends — passed, failed, errored or stopped early — `da` closes every
tab in the set, including a tab that was opened after the last step ran. A case
that starts its own browser closes the browser, as before. With `connect`, only
those tabs are closed and the user's browser keeps running. A `da session` keeps
the same set in its saved handle, so `browser.switch_tab` in one `session step`
command stays in effect for the next command, and `da session stop` closes every
tab the session opened.

The step is not `retry`-able: it changes the tab the session drives. It cannot
reach a page target this case does not own, so a tab the user drags into the same
browser, or a popup opened by a page outside this case, is out of reach by
design.

`browser.type` keeps its original `text` argument as the text to type. Because
`text` is also a locator, the two meanings are separated by `value`: when `value`
is given, `text` names the element and `value` is typed. These two steps are
equivalent except for how the element is found:

```yaml
- browser.type:
    css: "#name"
    text: Ada

- browser.type:
    text: Editor
    value: Ada
```

`browser.open` returns the URL the page actually reached, after redirects, and
its title. Its own `timeout_ms` bounds how long the page may take to finish
loading; exceeding it is a timeout (exit 5).

Every wait and interaction a browser step makes on the page — navigation, the
load event, the locator and actionability polls, `Input.*`, `browser.scroll`,
`browser.hover`, `browser.drag`, `browser.set_clipboard`,
`browser.read_clipboard`, `browser.wait`, the screenshot and the evidence
snapshot — ends at once as an environment error (exit 4) when Chrome reports
that the page's target crashed or detached, or when the owned tab is already
closed, and the message names the event or tab. A tab that dies while loading
is therefore not reported as a slow page, and steps stay strictly target-scoped
over CDP without moving the host OS cursor or touching unowned windows. Without
such an event the wait still ends at `timeout_ms` with the same exit code as
before, and the timeout message says that no target/session event arrived.

`browser.press` accepts `Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, the four
arrow keys, `Home`, `End`, `PageUp`, `PageDown`, `Space`, or one printable
character. Any other key name is an argument error that `da lint` reports.

`browser.screenshot` writes a PNG to `path` and returns the path and the number
of bytes written. A relative path is resolved against the directory that holds
the case file, so a case can keep its output beside itself. The step covers every
visible editable field before capturing: `textarea`, `select`, elements whose
`contenteditable` is on, and every `input` whose type is not `button`, `submit`,
`reset`, `checkbox`, `radio`, `image`, `hidden`, `range` or `color`. Each field
gets an opaque black box at its viewport position with the highest `z-index`;
the boxes are removed immediately after the capture, and also when the capture
fails. There is no option to turn the cover off. Use `browser.read` when the value
of a field is what the case needs to assert.

Screenshots stay local. `browser.screenshot` writes only the path the case gives,
and a failure bundle writes its copy under the bundle directory. Neither copy
enters text output, `--output json` or a future shareable report.

The cover hides editable fields, not ordinary text. A page that writes a secret
into a heading, a paragraph or an attribute still shows it in the screenshot
pixels; `dom.html` is redacted, but the PNG is not. The cover also descends into
accessible same-origin and nested frames (`<iframe>` and `<frame>`) and into open
shadow roots, scaling masks across axis-aligned frame transforms
(`scale(...)`, `translate(...)`, `zoom`), and refuses to emit a screenshot when a
visible field lies inside an unmappable transform (such as rotation, skew,
reflection, or 3D transforms), whereas cross-origin frames cannot be reached from
the main page. Treat both kinds of page as unsuitable for screenshots that leave
the machine.

`headless`, `width` and `height` only apply when the first `browser.open` creates
the page. A later `browser.open` that gives a different effective value is a step
error instead of being silently ignored, so one case cannot change the browser it
is driving halfway through. `timeout_ms` applies to each navigation, so a later
`browser.open` may choose a different value.

#### Scrolling, hovering, dragging, and scoped clipboard

All four Computer Use primitives run strictly inside the owned CDP page target —
they never move the host OS cursor, hijack unrelated desktop windows, or read/write
arbitrary `Runtime.evaluate` strings.

- **`browser.scroll`** scrolls the viewport (`delta_x` / `delta_y`), dispatches
  a CDP wheel event at `at` coordinates (`at` + `delta_x` / `delta_y`), scrolls a
  uniquely matched scrollable container (`locator` + `delta_x` / `delta_y`), or
  scrolls a matched element (and its enclosing same-origin frames) into view
  (`locator` with `into_view: true` or omitted deltas). In `da session`, `ref`
  targets compile into verified deterministic locators on `da session export`.
- **`browser.hover`** waits for full actionability on a uniquely matched element
  (or validates live viewport bounds on `at`), dispatches a CDP pointer move to
  its center, and triggers CSS `:hover`, `mouseover`, and `mouseenter` events.
  Returns `{ "hovered": true }` plus optional `text` (or `x`, `y`, `viewport`
  for `at`).
- **`browser.drag`** resolves `from` and `to` (each a locator, a session `ref`,
  or explicit `at` coordinates), re-verifies `from` if scrolling `to` shifted the
  page, and drives a stepped CDP pointer drag with `Input.setInterceptDrags` and
  `Input.dispatchDragEvent` (`dragEnter`, `dragOver`, `drop`), handling both
  pointer-event controls and HTML5 `draggable` elements across main documents,
  open shadow DOM, and same-origin `<iframe>`s. Using `at` on either endpoint
  emits a `da lint` warning and pins `viewport` during `da session export`.
- **`browser.set_clipboard` and `browser.read_clipboard`** read and write text
  in the browser-scoped clipboard via `navigator.clipboard` (granting
  `clipboardReadWrite` / `clipboardSanitizedWrite` on the page origin and
  enabling focus emulation on that page target). Both steps require a secure
  context (`http://127.0.0.1`, `http://localhost`, `https://`, or `file://`) and
  fail clearly on `about:blank`.
  - `browser.set_clipboard` accepts `text` (including `${env:NAME}` secrets),
    returns only `{ "characters": N }` without echoing the text, and cannot use
    `retry`.
  - `browser.read_clipboard` returns `{ "text": ... }` and is safe to pair with
    `retry`. Any value read from `${env:NAME}` is redacted as `***` across text
    output, `--output json`, `steps.jsonl`, `state.json`, and failure bundles.
  - **External browser (`connect`) boundary:** clipboard values preexisting on
    an externally connected user browser (`browser.open: { connect: ... }`) before
    `da` starts did not come from `${env:NAME}`. Just like preexisting cookies
    or storage in a connected browser, `da` never read those values from the
    environment and cannot redact them; connected runs mark their evidence
    `local_only`.

```yaml
name: Hover, drag, scroll, and copy with secret redaction
steps:
  - browser.open:
      url: http://127.0.0.1:8080/board
  - browser.hover:
      role: button
      name: Card menu
  - browser.drag:
      from: { testid: task-1 }
      to: { testid: done-column }
  - browser.scroll:
      testid: audit-log
      delta_y: 240
  - browser.set_clipboard:
      text: "${env:API_TOKEN}"
  - browser.read_clipboard: {}
    retry:
      until:
        - path: $.text
          exists: true
      timeout_ms: 2000
      interval_ms: 50
```

#### Uploading files (`browser.upload`)

`browser.upload` sets the files of an `<input type="file">` element through the
DevTools protocol (`DOM.setFileInputFiles`), so Chrome fires the same `input` and
`change` events a real file picker would. It returns `files` — how many files the
input now holds — and `names`, their base names. Only the base names are
returned, never the local directory, so a report stays portable.

```yaml
name: Submit an avatar
steps:
  - browser.open:
      url: http://127.0.0.1:8080/profile
  - browser.upload:
      text: Choose avatar
      files: ["fixtures/avatar.png"]
  - browser.click:
      role: button
      name: Save
```

- `files` is required. A relative path resolves against the directory that holds
  the case file (under `da session`, the working directory), the same rule
  `browser.screenshot` uses for `path`; an absolute path is used as given.
- `files: []` clears the current selection. The step removes the files itself,
  because Chrome leaves the old selection in place when an empty list is sent to
  `DOM.setFileInputFiles`; it then dispatches `input` and `change`, so a page
  that reacts to a clear sees one. An input that held nothing stays quiet.
- Every locator works — `css`, `xpath`, `text`, `role` and `testid` — plus
  `within`, `nth` and `timeout_ms`. The element must exist and be enabled, but it
  does **not** have to be visible: `display: none`, `opacity: 0`, zero-size and
  label-associated file inputs all work, because that is how real pages hide the
  native control.
- A file input inside an open shadow root is reached by `css`, `xpath` or
  `testid` like any other shadow element, and the step always applies to the tab
  the session is on: after `browser.switch_tab`, the upload targets the new tab's
  input.
- A locator that matches a `<label>` uploads to the control the label names, so
  `text: "Choose avatar"` reaches the input it hides. This is the step's only
  retargeting: every other element that is not an `<input type="file">` is
  refused.
- Chrome does not return file inputs for an accessibility query filtered by
  accessible name, so a `role` locator reaches one by role alone; narrow it with
  `within` or `nth` when the page has more than one file input.
- `browser.upload` is not a click: nothing is opened, and the page's submit
  button still has to be clicked to send the file.

The step fails with a message that names what to change when a path does not
exist, a path is a directory or another special file, the matched element is not
an `<input type="file">` (the message names the tag and the input type), the
input is disabled (its own attribute or an enclosing disabled `<fieldset>`), or
more than one path is given for an input without the `multiple` attribute. Every
path is checked by `da` before Chrome is asked, so a missing fixture is reported
as itself instead of as an opaque browser error.

#### Shadow DOM

`css`, `testid`, `text` and `role` reach elements inside **any number of nested
open** shadow roots, so a case can drive Web Component internals directly:

```yaml
- browser.click:
    css: "#confirm-button"             # an id inside the component's open root
- browser.click:
    testid: shadow-field
- browser.click:
    text: Pay now
- browser.click:
    role: button
    name: Place order
    within:
      css: "#card-widget"              # the component's host element
```

The search walks the composed tree in pre-order: each element, then the contents
of its open `shadowRoot`, then its light children. `nth` counts in that same
order, and `within` accepts a shadow host — `within: { css: "#card-widget" }`
searches that host's open root — alongside same-origin iframes. A `role` lookup
already sees shadow content through Chrome's accessibility tree, and `within`
narrows it to the host's composed subtree.

Actionability applies the same rule. `document.elementFromPoint` reports the
outermost shadow host at a point, so the hit test descends each open root in turn
to find the element that really receives the click. A control inside a shadow
root is therefore no longer reported as "occluded by" its own host, while a real
overlay above it still is, and an `<iframe>` inside a shadow root is checked in
the document that contains its host.

`browser.screenshot` masking covers editable fields inside open shadow roots the
same way it covers fields inside iframes.

**Closed roots cannot be pierced.** A root created with
`attachShadow({ mode: 'closed' })` is invisible to page scripts
(`host.shadowRoot` is `null`), so no locator can search it. When a lookup times
out or a `role` click cannot land, `da` checks the document through the DevTools
protocol and names the closed root in the error, for example:

```text
could not find CSS selector `#closed-button` within 400 ms; check the page and the locator; the page contains a closed shadow root, which `da` cannot pierce: open it with `attachShadow({ mode: 'open' })` or select its host
```

To test such a component, change the root to `open`, or select an element the
component exposes in the light DOM (its host, a slot, or a `part`).

**`xpath` stays shadow-blind.** `document.evaluate` cannot enter a shadow root,
so XPath keeps matching only the node's own document tree. Use `css`, `testid`,
`text` or `role` when a component's internals matter, and treat an XPath
expression as a main-document-only locator.

#### JavaScript dialogs (`alert`, `confirm`, `prompt`, `beforeunload`)

By default, an unexpected browser dialog never waits out `timeout_ms`: as soon as
Chrome opens an `alert`, `confirm`, `prompt` or `beforeunload` dialog, the step
immediately dismisses it (`Page.handleJavaScriptDialog` with `accept: false`) to
keep the page responsive for failure evidence, subsequent steps and session
close, and fails at once as an execution error (exit 1) whose message names the
dialog `type` and its `message` (redacted like any other step output when it
contains a `${env:...}` secret).

To handle an expected dialog instead, set `dialog` on `browser.open`,
`browser.click`, `browser.type`, `browser.press`, `browser.wait`,
`browser.scroll`, `browser.hover` or `browser.drag`:

- `dialog: accept` or `dialog: dismiss` — accepts or cancels the dialog.
- `dialog: { action: accept, prompt_text: "..." }` or
  `dialog: { action: dismiss }` — specifies the action and, when accepting a
  `prompt`, the text to submit (`prompt_text`).

When a step with `dialog` configured handles a dialog, it succeeds and includes
`dialog` in its JSON result (`{ "type": ..., "message": ..., "accepted": ... }`),
so `expect` can assert the dialog kind, text and decision. Dismissing a
`beforeunload` dialog on `browser.open` keeps the browser on the current page.

A dialog that opens late is why `browser.wait` may combine `dialog` with
`retry`: every polling attempt handles the dialogs of its own attempt, so
`until` can assert `$.dialog.type` on the attempt that finally sees the dialog.
`browser.read` takes no `dialog` argument, so an unexpected dialog ends that
attempt at once as an execution error; because an execution error is retryable,
a later attempt still succeeds once the dialog is gone.

```yaml
name: Confirm deletion and enter a prompt value
steps:
  - browser.open:
      url: http://127.0.0.1:8080/items
  - browser.click:
      role: button
      name: Delete item
      dialog: accept
    expect:
      - path: $.dialog.type
        equals: confirm
      - path: $.dialog.message
        contains: Delete
      - path: $.dialog.accepted
        equals: true
  - browser.click:
      role: button
      name: Rename item
      dialog:
        action: accept
        prompt_text: updated-name
    expect:
      - path: $.dialog.type
        equals: prompt
      - path: $.dialog.accepted
        equals: true
```

#### Connecting to a browser that is already open

Start Chrome with a debug port, then name that endpoint in `browser.open`:

| System | Command |
| --- | --- |
| Windows | `"C:\Program Files\Google\Chrome\Application\chrome.exe" --remote-debugging-port=9222` |
| macOS | `"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --remote-debugging-port=9222` |
| Linux | `google-chrome --remote-debugging-port=9222` |

```yaml
name: Use the browser I am already signed in to
steps:
  - browser.open:
      connect: "http://127.0.0.1:9222"
      url: http://127.0.0.1:8080/orders
  - browser.click:
      role: button
      name: New order
  - browser.read:
      text: "Order created"
```

`connect` must be `http://<host>:<port>` and the host must be `127.0.0.1`,
`localhost` or `[::1]`; any other host is an argument error, because a remote
debug port would hand the browser to another machine. The step reads
`GET /json/version` at that address to find the browser's DevTools WebSocket,
opens a new tab and drives it like a browser the case started itself. Only the
tab this case opened is closed when the case ends; the user's browser and its
other tabs keep running. `connect` cannot be combined with `headless`, `width`
or `height`, and a second `browser.open` in the same case must give the same
`connect` value.

A debug port is full control of the browser: every cookie, every signed-in
session and every page it can reach. Keep it on loopback, do not expose it on a
network interface, and close Chrome when you no longer need it. Newer Chrome
versions refuse a debug port on the default profile; if yours does, start Chrome
with a dedicated `--user-data-dir` that you have signed in to, and treat that as
a separate profile from your everyday one. If the port is closed or the address
is not a DevTools endpoint, `browser.open` is an environment error (exit 4) whose
message names `--remote-debugging-port`.

#### Reusing a login across runs

Most suites start after signing in. Typing the password into the page on every
run is slow, breaks when the account has a second factor or a captcha, and puts
the password on screen. There are four ways to bring a login into a run; pick the
one that matches what has to be shared.

| Way | Use when | Where the state lives |
| --- | --- | --- |
| Storage-state file (`browser.save_state`, then `browser.open: { storage_state: ... }`) | An automated run can sign in once and later runs should start signed in | A JSON file the caller names |
| Named profile (`browser.open: { profile: ... }`) | A person signs in once, or the app needs service workers, IndexedDB or other profile-scoped state | `.da/profiles/<name>/` |
| Connected browser (`browser.open: { connect: ... }`) | The everyday browser is already signed in | The user's own browser |
| Cookie injection (`browser.set_cookies`) | The login is an HTTP API whose token a case already holds | The running browser session only |

##### Storage-state file

`browser.save_state` writes the cookies of the current page and the current
origin's `localStorage` to a JSON file. A later, independent run names that file
in `browser.open` and the page is already signed in when it loads — no typing, no
cookie jar of the browser to keep.

```yaml
name: Sign in once and save the state
steps:
  - browser.open:
      url: http://127.0.0.1:8080/login
  - browser.type:
      css: "#user"
      text: alice
  - browser.type:
      css: "#password"
      text: "${env:DA_PASSWORD}"
  - browser.press:
      key: Enter
  - browser.wait:
      url_contains: /account
  - browser.save_state:
      path: state/session.json
```

```yaml
name: Start signed in from the saved state
steps:
  - browser.open:
      url: http://127.0.0.1:8080/orders
      storage_state: state/session.json
  - browser.read:
      css: "#who"
    expect:
      - path: $.text
        equals: "signed in as alice"
```

The file has the shape `{"version": 1, "cookies": [...],
"local_storage": {"<origin>": {"<key>": "<value>"}}}`. Its `cookies` entries use
the same fields as `browser.set_cookies`, so a file can also be pasted into a
case that injects cookies directly.

`browser.save_state` returns only `path` and the counts `cookies` and
`local_storage`; no value ever enters standard output, `--output json`, a failure
bundle or a report. A relative `path` is resolved against the directory that
holds the case file, like `browser.screenshot`. On Unix the file is written with
mode `0600`; on Windows it inherits the directory's access control list. The file
is a credential: keep it out of version control and out of shared artifacts.

Limits worth knowing:

- `local_storage` is saved for the origin of the page that is open when
  `browser.save_state` runs. Sign in, then save, in that order.
- Cookies with an empty value are not written; when nothing at all is left to
  save, the step fails instead of writing an empty file.
- Restored cookies become session cookies in the new browser: the original
  expiry is not saved, so the reused login lasts for that browser's lifetime.
- The step needs an open `http://` or `https://` page and refuses a browser
  reached through `connect`, because that browser holds your everyday cookies.

`browser.open` reads the file **before** launching the browser and **before** the
navigation: the browser starts only after the file was read and validated, and
the cookies and `localStorage` are applied while the page is still `about:blank`.
A file that is missing, unreadable, larger than 8 MiB, not valid JSON, of an
unsupported `version`, or carries no usable cookies and no `localStorage` fails
the step as an environment error (exit 4). The message names the file and the
problem and never repeats a cookie or token value, so a public log stays safe.
Because the file must exist at run time, `da lint` checks the argument shape but
not the file.

The argument is an ordinary string, so a variable may name the file:

```yaml
steps:
  - browser.open:
      url: ${base_url}/orders
      storage_state: ${state_file}
```

```bash
da run tests/orders --var base_url=http://127.0.0.1:8080 --var state_file=state/admin.json
```

Inside `da session`, the same two steps are available step by step:

```bash
da session step "$id" browser.save_state --arg path=state/session.json
da session step "$id" browser.open --with '{"url": "http://127.0.0.1:8080/orders", "storage_state": "state/session.json"}'
```

##### Named profile

`browser.open: { profile: admin }` starts Chrome with the persistent user data
directory `.da/profiles/admin/` instead of a temporary one. Sign in once with a
visible window, and every later run of that case — in another process, on another
day — starts with that profile. Unlike a temporary profile, the directory is
never removed when the case or the session ends.

```yaml
name: Sign in by hand once, then reuse it
steps:
  - browser.open:
      url: http://127.0.0.1:8080/account
      profile: admin
      headless: false
  - browser.read:
      css: "#who"
```

The name may contain ASCII letters, digits, `-` and `_`, and must not be empty.
The default root is `.da/profiles` under the working directory; `DA_PROFILES_DIR`
overrides it with another directory (an absolute path or one relative to the
working directory). The root and the profile directory are created when missing
and refused when either is a symlink or not a directory.

One profile is used by one browser at a time. A second case, session or process
that names the same profile while the first still has it open fails at once with
`browser profile `<name>` is already in use`, instead of starting a second Chrome
on the same directory and corrupting it. The lock lives in
`.da/profiles/<name>.lock` and is released when the browser closes; a stale lock
from a crashed run is reclaimed after checking that no browser process still uses
the profile.

A profile holds whatever the browser stored, including cookies and tokens that
`da` never read. See [unmanaged state](#evidence-from-a-profile-or-a-connected-browser)
for what that means for evidence.

##### Cookie injection

When the login is an API call, let `http.request` fetch the token and put it into
the browser instead of driving the form. `browser.set_cookies` writes cookies
through the DevTools protocol and optional `localStorage` entries, and works
before the first `browser.open`:

```yaml
name: Inject an API login before opening the protected page
steps:
  - http.request:
      method: POST
      url: http://127.0.0.1:8080/api/login
      json:
        user: alice
        password: "${env:DA_PASSWORD}"
    save:
      session: $.json.token
  - browser.set_cookies:
      cookies:
        - name: sid
          value: ${session}
          url: http://127.0.0.1:8080/
          http_only: true
          same_site: Lax
      local_storage:
        "http://127.0.0.1:8080":
          auth_token: ${session}
  - browser.open:
      url: http://127.0.0.1:8080/orders
```

Every cookie needs a non-empty `name` and `value` and either `url` or `domain`;
an empty cookie array or an empty `local_storage` map is an argument error. The
step returns `cookies_set` and `local_storage_set` counts only. A cookie name and
value appear in a case file only as a `${var}`, and the value is redacted from
output like any other secret (see [Secrets](#secrets)).

`local_storage` is keyed by an origin or by `current`:

- A key of `current` writes to the page that is open now; with no HTTP(S) page
  open yet, the step fails at run time instead of guessing an origin.
- A full `http://` / `https://` origin registers a one-shot script that writes
  those keys when a document of that origin loads, so it also works before the
  first `browser.open` of that origin.

##### Which one wins

- `connect` cannot be combined with `profile` or with `storage_state`; each pair
  is an invalid-argument error reported by `da lint` (and refused before any step
  runs under `da run`), and by `da session step` as exit 2. A connected
  browser is somebody's everyday browser: a state file or a named profile would
  write into it.
- `profile` and `storage_state` may be combined. The browser starts from the
  profile directory, then the state file is applied before the navigation. For a
  cookie with the same name, domain and path, and for a `localStorage` key that
  already exists, the state file wins for the page being opened. Profile state
  the file does not mention stays as the profile left it.
- `storage_state` is applied on every `browser.open` that names it, before that
  navigation. `headless`, `width`, `height`, `profile` and `connect` are fixed
  when the browser is created, like every other `browser.open` option.
- A `browser.set_cookies` step overwrites either source from the moment it runs,
  so it stays the last word for that session.

##### Evidence from a profile or a connected browser

A browser started with `profile` or reached with `connect` may show pages that
were signed in outside `da` (and a `connect` browser may already hold preexisting
clipboard contents created before `da` started), so `da` cannot know every
secret its evidence could contain. Preexisting clipboard values on a connected
user browser did not come from `${env:NAME}` and therefore are not redacted by
`Secrets` (the same boundary as preexisting cookies and storage). Such a case
marks its evidence `local_only`: failure bundles write
`"local_only": true` with a `"local_only_reason"`, text output prints a
`local-only:` line, and `--output json` adds `local_only` and
`local_only_reason`. A browser `da` starts in a temporary directory has none of
that state; a loaded `storage_state` file does not mark the evidence local-only,
because every value in it came through a file `da` read. Keep local-only evidence
on the machine — `da report --shareable` omits its screenshots.

Chrome 136 and later ignore `--remote-debugging-port` for the default user data
directory, so `connect` cannot take over the Chrome you browse with. Start a
separate Chrome with its own `--user-data-dir` for `connect`, or let `da` manage a
named `profile` instead.

The browser is closed when the case ends, whether it passed, failed an
assertion, errored or timed out. A session belongs to one case, so `--jobs 4`
gives four running cases four browsers. A case that never calls `browser.open`
never starts one. A connected browser is the exception: the case closes only its
own tab and never sends `Browser.close`.

Chrome is found in this order:

1. `DA_CHROME`, an absolute path to the `chrome`, `chromium` or `msedge`
   executable.
2. The usual install locations: on Windows Chrome, Chromium and Edge under
   `ProgramFiles`, `ProgramFiles(x86)` and `LocalAppData`; on macOS
   `/Applications/Google Chrome.app` and `/Applications/Chromium.app`; on Linux
   `/usr/bin/google-chrome`, `/usr/bin/chromium` and `/snap/bin/chromium`.
3. These program names on `PATH`: `google-chrome`, `google-chrome-stable`,
   `chromium`, `chromium-browser` and `chrome`.

A case that does not set `timeout_ms` on `browser.open` gets thirty seconds for
the navigation. `DA_BROWSER_NAVIGATION_TIMEOUT_MS` raises or lowers that default:
a value from 1000 to 600000 is used, anything else falls back to thirty seconds.
It exists for machines that run several browsers at once, where a page that loads
instantly on an idle machine can exceed the default only because the runner is
contended. The value never overrides a `timeout_ms` a case names.

`DA_BROWSER_LAUNCH_TIMEOUT_MS` does the same for the launch budget: the time
`da` gives Chrome to start, open its debugger port and answer the first
commands. It defaults to thirty seconds, accepts 1000 to 600000, and falls back
to the default otherwise.

Each case that starts its own browser gets a fresh temporary profile and a
debugger that listens on a loopback port the operating system chooses, so two
cases at the same time cannot share a profile or a port. A case that names a
`profile` uses that persistent directory instead, and the lock described above
keeps two cases from driving one profile at the same time. Connected cases share
the user's browser but each one opens and closes its own tab.

A headless browser runs without GPU acceleration: it composites in software and
WebGL uses a software ANGLE renderer, so a page that depends on the GPU can draw
more slowly and its pixels can differ from the hardware path. That is a
deliberate trade. On a machine that runs several browsers at once, the GPU
process was using about 9 s of a CPU core for four cases, and the first
navigation could spend its whole 30 s budget before the request was written;
without GPU acceleration the same four cases used about 1 s of a core for it and
finished. A browser with a visible window keeps its GPU. Neither mode changes how
the system proxy is resolved, so a case behind a corporate proxy works the same.

When no browser is found, or it cannot start, the step is an environment error
(exit 4) whose message names `DA_CHROME`. An element that does not appear within
`timeout_ms` fails the step like a failed assertion (exit 1) and the message
contains the locator and the timeout. The text `browser.type` sends and
`browser.read` returns is redacted like every other value in output and failure
bundles. See [Secrets](#secrets).

```yaml
name: Greeting is shown
steps:
  - browser.open:
      url: http://127.0.0.1:8080/login
    expect:
      - path: $.title
        equals: Sign in
  - browser.type:
      css: "#user"
      text: alice
  - browser.type:
      xpath: "//*[@id='password']"
      text: "${env:DA_PASSWORD}"
  - browser.press:
      key: Enter
  - browser.wait:
      css: "#welcome"
  - browser.read:
      css: "#welcome"
    expect:
      - path: $.text
        equals: "Welcome, alice"
  - browser.read:
      css: "#welcome"
      attribute: data-state
    expect:
      - path: $.value
        equals: ready
```

#### Observing the network

`browser.network` reports the requests the page finished while a step ran. It
takes no arguments and is declared like any other observer:

```yaml
observe:
  net:
    browser.network: {}
```

The value appears under `observed.net` on every step result:

```json
{
  "requests": [
    {
      "method": "POST",
      "url": "http://127.0.0.1:1234/api/login",
      "status": 200,
      "type": "Fetch",
      "failed": null
    }
  ],
  "pending": 0,
  "dropped": 0
}
```

| Field | Meaning |
| --- | --- |
| `requests` | Requests that finished during the step, in the order they finished. `method`, `url` and `type` come from the request; `status` is the response status, and is `null` when the request failed. `failed` is Chrome's `errorText` for a failed request and `null` otherwise. |
| `pending` | Requests that were still in progress when the step ended. They are reported in a later step, when they finish. |
| `dropped` | Messages and requests discarded during this step to keep the connection bounded. |

`Network.enable` is sent when the page is attached, before the first navigation,
so the document request of the `browser.open` step is part of that step's
`requests`. A request is reported in the step that **finished** it, not the step
that started it: a response that arrives after a click is part of the following
`browser.wait` step. A request is reported once; a step that starts no request
and finishes none reports an empty list.

A normal `expect` path filters by URL and asserts the status. The filter selects
exactly one value, so a duplicate report would fail the assertion too:

```yaml
name: Login calls the API once
observe:
  net:
    browser.network: {}
steps:
  - browser.open:
      url: http://127.0.0.1:8080/login
  - browser.click:
      css: "#submit"
  - browser.wait:
      css: "#welcome"
    expect:
      - path: "$.observed.net.requests[?(@.url == 'http://127.0.0.1:8080/api/login')].method"
        equals: POST
      - path: "$.observed.net.requests[?(@.url == 'http://127.0.0.1:8080/api/login')].status"
        equals: 200
      - path: $.observed.net.pending
        equals: 0
```

`data:` and `blob:` URLs are generated by the page rather than fetched from a
server, so they are not reported. The observer records no request headers and no
response body: cookies, authorization headers and tokens never enter an
observation, a report or a failure bundle. URLs are redacted like every other
value, so a `${env:...}` value that appears in a query string is replaced by the
redaction marker in output and bundles.

Four bounds keep a long-running case finite: the connection keeps at most
10,000 received messages, one collection spends at most 200 ms reading the
socket, the page tracks at most 1,000 unfinished requests, and it keeps at most
1,000 finished requests for the next collection. When a bound is reached the
oldest entry is discarded and `dropped` grows, so a case can tell that its
observation may be incomplete instead of failing silently or growing without
limit. The time bound exists because a page can send events faster than the
socket read times out; whatever a collection did not read stays on the socket
and is read by the next one, so a busy page delays an observation rather than
stalling the step that collects it. Only the page this case attached to is
observed; another tab of the same browser contributes nothing.

### Android steps

The `android.*` steps drive one Android device through the `adb` binary that is
already installed on the machine. They are provided by the `android` feature and
never install anything on the device. Two settings reach it:

| Setting | Meaning |
| --- | --- |
| `DA_ADB` | The adb binary to run. Defaults to `adb`, looked up on `PATH`. Set it to the adb executable when the platform-tools directory is not on `PATH`. |
| `DA_ANDROID_SERIAL` | The device to drive when more than one is attached. A step's `serial` argument overrides it for that step. |

```sh
export DA_ADB="$HOME/Android/Sdk/platform-tools/adb"
export DA_ANDROID_SERIAL=emulator-5554
```

Every step spawns `adb`: one command for an action and three for a dump, which
runs it, reads the file it wrote and removes it again. No device state is kept
between steps, so there is no session to reattach to either.

#### The dump costs two seconds, so no action reads the tree

Measured on the Android 14 container (`redroid14_x86_64`, 720×1280, density 320):
`uiautomator dump` takes about **2.0 s per call** and the cost is fixed —
`--compressed` halves the output to 9.4 KB and changes nothing, and `exec-out`
without the intermediate file is no faster — while `input tap` takes **24 ms**,
`screencap -p` about 160 ms and `dumpsys window windows` 23 ms, the last of
which exposes only window rectangles and no element text.

A driver that dumped before every action would therefore cost two seconds per
step. This one never does:

- `android.tap`, `android.input_text` and `android.screenshot` are one adb call
  each and read no UI tree.
- `android.dump` pays the two seconds once and is the only step that returns
  text, resource ids, content descriptions and bounds.
- `android.tap_on` pays them knowingly: one dump, one resolution, one tap.

A case that already knows the pixels from an earlier dump taps the pixels. A
case that needs semantics — "tap the button whose text is Save" — pays for one
dump, and `android.dump` is the only Android step that is read-only enough for
[`retry`](#retrying-read-only-steps), so polling for a screen that is still
loading is a `retry` on the dump.

#### Steps

| Step | Arguments | Result |
| --- | --- | --- |
| `android.launch` | `package` and/or `activity`; optional `serial`, `timeout_ms` | `package`, `component` |
| `android.tap` | `x`, `y` in pixels; optional `serial`, `timeout_ms` | `x`, `y` |
| `android.input_text` | `text`; optional `serial`, `timeout_ms` | `characters` |
| `android.screenshot` | `path`; optional `serial`, `timeout_ms` | `path`, `bytes` |
| `android.dump` | optional `text` or `resource_id`; optional `serial`, `timeout_ms` | `count`, `nodes`, and `target` when a filter is given |
| `android.tap_on` | `text` or `resource_id`; optional `serial`, `timeout_ms` | `target`, `center` |

`android.launch` starts `package` with `am start -W -n package/activity` when
an activity is named. `-W` waits until the launch finishes, so the step after
the launch sees the screen it produced. `am start` needs a component, so a
package alone is started through the same `MAIN`/`LAUNCHER` intent with
`cmd package resolve-activity`, which resolves the launcher activity on the device, and then `am start -W -n` on the resolved component; pass `activity`
when the entry point is not the launcher, or when the next step must wait for
the first screen. `am start` exits 0 even when it refuses a component, so a
device answer containing `Error:` fails the step instead of passing it.

`android.tap` taps pixels. Neither it nor any other action reads the tree first.

`android.input_text` types into the field that currently has focus. Spaces
become `%s`, which is how `input text` carries them, and the text is quoted for
the device shell, so `$`, `;`, quotes and backslashes reach the field unchanged.
The step refuses empty text, control characters and `%`: `input text` sends key
events, so it has no key for a newline or a tab, and it reads `%s` as a space
with no escape for a literal percent sign. The text is not echoed in the result
or in an error message, including a timeout: that error names the command as
`adb shell input text <redacted>`. The result is the number of `characters` sent,
so a password stays out of the report.

`android.screenshot` writes the PNG of the screen. A relative `path` resolves
against the directory of the case file, the way every other output path in the
case does, and the device answer is checked for the PNG signature before it is
written.

`android.dump` runs `uiautomator dump` once, reads the file it wrote in
`/data/local/tmp` and removes it again, and returns the parsed nodes in document
order. Each node carries `text`, `resource_id`, `content_desc`, `class`,
`bounds` (`left`, `top`, `right`, `bottom` in pixels) and `center` (`x`, `y`);
an attribute the device left empty is `null`. With `text` or `resource_id` the
step also resolves exactly one node and returns it as `target`, centre included,
so a case can assert on it or tap it. A filter that names no node, or more than
one, fails the step: the message gives the node count, or up to five candidates
with their centres, because tapping the first of two `OK` buttons is how a case
passes while testing the wrong screen.

`android.tap_on` does what a case would otherwise do in three steps: dump,
resolve the one `text` or `resource_id`, tap the centre of `bounds`, and return
the `target` and the `center` it tapped.

```yaml
name: Search the Settings app
steps:
  - android.launch:
      package: com.android.settings
  - android.dump:
      text: Search settings
    save:
      search_bar: "$.target.center"
  - android.tap_on:
      text: Search settings
  - android.input_text:
      text: network
  - android.screenshot:
      path: shots/settings-search.png
```

Two YAML details matter here. A step with no required argument is written as
`android.dump: {}`, because an empty step value is not a mapping. And the case
format keeps argument keys as text, so an unquoted `y` is the vertical pixel
rather than the YAML 1.1 boolean; both spellings are read the same way:

```yaml
- android.tap:
    x: 360
    "y": 640
```

The same rule applies to values: type the letter `y` as
`text: "y"`.

#### Every step has a deadline

A device that stops answering holds an adb command open for as long as nobody
kills it: a dropped transport, a `uiautomator dump` that never returns or a
wedged input method all leave the step waiting until CI or the operator ends the
run. Every `android.*` step therefore accepts `timeout_ms`, the deadline of one
adb command:

| Field | Meaning |
| --- | --- |
| `timeout_ms` | Deadline of one adb command, in milliseconds. Defaults to `30000`. `0` and values above `600000` (ten minutes) are refused by `da lint`: a command that cannot start and a deadline that outlives the run both rescue nothing. |

On the measured container a dump costs about 2.0 s and a tap 24 ms, so thirty
seconds leaves ample room for a slow image while still failing fast on a device
that is gone. A step that runs more than one command — a dump runs three — gives
each of them the same deadline: the deadline bounds one adb call, not the case.

When the deadline passes the adb process tree is killed and reaped — its own
process group on Unix, its process tree on Windows — and both output pipes are
drained within one bounded one-second grace period, so a descendant holding a
pipe cannot keep the run waiting. The step fails with exit code 5:

```text
`android.dump` did not finish within its 30000 ms deadline: the adb command `adb shell uiautomator dump /data/local/tmp/da-dump-4231-0.xml` was killed after 30012 ms; raise `timeout_ms` if the device is only slow, or check that the device answers `adb devices`
```

The message names the step, the deadline, the time that passed and the command it
ran, and never the adb path, the serial or the text typed by
`android.input_text`, for the same reason the other messages do not. What the
command printed before it was killed is kept in the message too: a device that
dropped its transport says so there, while a device that is merely slow prints
nothing and only needs a larger `timeout_ms`.

#### A failed step records what the device showed

When a step fails and the device still answers, the step writes two files into
the case directory — the directory a relative `android.screenshot` path resolves
against — and names both in the error:

| File | Content |
| --- | --- |
| `android-failure-<pid>-<n>-<step>.png` | The screen as a PNG, captured the way `android.screenshot` captures it. |
| `android-failure-<pid>-<n>-<step>.xml` | The `uiautomator dump` document, so the tree can be read, parsed or diffed as XML. |

The two files of one failure share their name; the process id and the counter in
it keep two failures of one run — a `retry` on the dump, or two cases on the
same device — from overwriting each other's evidence. The error reads like this,
after whatever reason the step itself gave:

```text
no node with text `Save` in the current UI tree (37 nodes); check the value, or run `android.dump` without a filter to see the tree; the device still answered, so the failure was recorded in the case directory: screenshot `cases/shop/android-failure-4231-0-android.dump.png` (28416 bytes) and UI tree `cases/shop/android-failure-4231-0-android.dump.xml` (9124 bytes)
```

The screenshot is the screen as it is: unlike `browser.screenshot`, nothing is
masked, so a case that types a secret into a field and then fails leaves that
field in the evidence.

A command that times out is not asked for evidence at all: it already spent its
deadline against a transport that did not answer, and its partial output and
deadline error are the record. For an execution failure, the screenshot is also
the question whether the device is still there, and when it does not come back as
a PNG the step fails with the output adb printed — no evidence files, and no
second full deadline spent. The two evidence commands use `min` of the step's
deadline and ten seconds each. An invalid argument never reached the device and
records nothing either.

#### When there is no device

No step turns a missing device into a skip. Each one fails with the action that
fixes it:

| Situation | Exit code | Message |
| --- | --- | --- |
| adb cannot be started | 4 | `could not run adb: ...` and the setting to fix |
| no device attached | 4 | `no Android device is attached: connect one over USB or start an emulator, then check \`adb devices\`` |
| a device is not ready | 4 | `the attached Android device is not ready (\`unauthorized\`): accept the USB debugging prompt ...` |
| several devices, no serial | 4 | `N Android devices are attached: set \`DA_ANDROID_SERIAL\` ...` |
| a configured serial matches nothing | 4 | `no attached Android device has the serial in \`DA_ANDROID_SERIAL\` ...` |
| a command outlived `timeout_ms` | 5 | `` `android.tap` did not finish within its 30000 ms deadline: the adb command `adb shell input tap 10 20` was killed after ... `` |
| an invalid argument | 3 | reported by `da lint` before anything runs |

The messages name the serial setting, never the serial itself and never the
`DA_ADB` path: a report and a failure bundle are shared, and a serial identifies
one physical device. When adb printed something before it gave up, the message
keeps it after the advice, so a device that left the bus mid-run is not reported
as one that was never attached.

The Android tests in `tests/android.rs` follow the browser suite: the schema,
lint, missing-adb and deadline checks run on every machine — the deadline and
no-evidence tests point `DA_ADB` at a script that waits or fails instead of
answering, so the kill path is proven without a device — and the tests that drive
a device are gated on `DA_TEST_ANDROID`, including the one that forces a failure
and asserts the screenshot and the UI tree it wrote. With the variable unset
those tests print why they skip; with `DA_TEST_ANDROID=1` a missing device fails
the suite instead.

### Desktop steps

The `desktop.*` steps drive native desktop GUI applications through an
accessibility tree rather than pixel guessing. They are provided by the
opt-in `desktop` cargo feature (`cargo build --features desktop`): Tier 1
implements Windows UI Automation (`#84`), while macOS and Linux backends are
tracked in `#88` (on non-Windows builds, `desktop.*` steps compile cleanly and
return an environment error referencing `#88`).

An in-process `COINIT_MULTITHREADED` worker thread executes bulk-cached UI
Automation tree walks (`UICacheRequest`) and simulates input under an exclusive
host-level desktop lock so concurrent `da` runs never interleave mouse or
keyboard actions.

#### Steps

| Step | Arguments | Result |
| --- | --- | --- |
| `desktop.launch` | `command`, optional `args`, `cwd`, `env`, `window_title_contains`, `timeout_ms` (default `30000`) | `pid`, `hwnd`, `title`, `window`, `viewport`, `owned` |
| `desktop.attach` | At least one of `pid`, `executable`, `title_contains`; optional `timeout_ms` (default `10000`) | `pid`, `hwnd`, `title`, `window`, `viewport`, `owned`, `local_only` |
| `desktop.click` | Locator (or `ref` in `da session`) or `at: { x, y, window? }`; optional `button` (`left`, `right`, `double`), `timeout_ms` | `clicked`, `button`, `role`/`name`/`automation_id` or `at`, `window` |
| `desktop.type` | Locator (or `ref` in `da session`) or `at`, plus `text` (or `value` when `text` locates the control); optional `timeout_ms` | `typed_chars`, `role`, `window` |
| `desktop.press` | `key` or chord (for example `Enter`, `Tab`, `Ctrl+S`, `Ctrl+Shift+P`); optional `timeout_ms` | `pressed` |
| `desktop.wait` | Locator (or `ref` in `da session`), optional `state` (`present`, `gone`, `enabled`, `visible`), `timeout_ms` | `matched`, `state`, `elapsed_ms`, `element` |
| `desktop.read` | Locator (or `ref` in `da session`), optional `timeout_ms` | `role`, `name`, `value` (`null` when `is_password`), `automation_id`, `class_name`, `bounds`, `enabled`, `is_password` |
| `desktop.screenshot` | `path`; optional `mask_inputs` (default `true`), `timeout_ms` | `path`, `width`, `height`, `masked_inputs`, `masked_count` |

- **Locators**: match controls by `automation_id`, `role` (`Button`, `Edit`,
  `Text`, `CheckBox`, `ComboBox`, `MenuItem`, `Pane`, `Window`, …), exact
  `name`, case-insensitive `text` substring, `class_name`, container scope
  `within`, and 0-based `nth`. Volatile WinForms `HWND`-derived
  `AutomationId` values (`AutomationId == NativeWindowHandle`) are filtered out
  automatically during snapshots and locator synthesis so exported cases stay
  portable across runs.
- **`desktop.attach` is `local_only`**: attaching to an already-running
  desktop window never kills that process on close and marks the run
  `local_only: true`, because pre-existing local window state can contain
  unmanaged data.
- **Sessions & export (`da session`)**: `desktop` sessions detach and reattach
  across CLI/MCP calls by persisted `pid`, `hwnd`, `owned_launch`, and kernel
  `process_start_filetime` (verified before reattaching or terminating an owned
  process so Windows PID reuse never touches an unrelated process).
  `da session snapshot` assigns bounded `e1`, `e2`, … refs; `da session export`
  synthesizes deterministic locators (and reverse-looks up `ElementFromPoint`
  when `at:` hit a named interactive control), skips bare `desktop.read` steps
  without `expect` or `save` unless `--include-reads` is passed, disambiguates
  `text` vs `value` on `desktop.type`, and pins `at.window` on remaining
  coordinate clicks.
- **Failure evidence & input masking**: on failure, the session captures
  `desktop/screenshot.png` (painting every `Edit` and `IsPassword` control's
  `BoundingRectangle` solid black by default) and `Secrets`-redacted
  `desktop/uia-tree.json` (never exposing password values).

```yaml
name: Native desktop login flow
steps:
  - desktop.launch:
      command: build/DemoApp.exe
      window_title_contains: Sign In
  - desktop.type:
      automation_id: usernameBox
      text: alice
  - desktop.type:
      automation_id: passwordBox
      text: "${env:DEMO_PASS}"
  - desktop.click:
      role: Button
      name: Submit
  - desktop.wait:
      automation_id: statusLabel
      text: Welcome
```

#### UIPI, elevation, and custom-drawn surfaces

- **UIPI and closed windows**: Windows User Interface Privilege Isolation
  (UIPI) blocks a standard medium-integrity `da` process from driving an
  elevated Administrator window (`E_ACCESSDENIED`, `0x80070005`). Because UIA
  also briefly returns `0x80070005` while tearing down a window that just
  exited, `da` checks child exit status and `element_from_handle` / process
  liveness before reporting a UIPI elevation mismatch — closed windows fail fast
  with `"desktop window closed"` instead of a misleading elevation error.
- **Canvas & custom-drawn widgets (`at:`)**: controls painted directly with
  GDI, Direct2D, or `DrawingContext` expose no child UIA nodes. Use
  window-relative `at: { x, y, window: { width, height } }` as an explicit
  escape hatch; `da lint` warns on `at` coordinates (and warns again when
  `at.window` is omitted), and `desktop.click` fails if the live window size
  differs from `at.window`.
- **Electron and WebView2 apps (`--remote-debugging-port`)**: Electron and
  WebView2 desktop apps render a Chromium DOM inside a native frame. Prefer
  launching them with `--remote-debugging-port=9222` (or setting
  `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222`) and
  attaching `browser.open` (`connect:`) for full DOM locators, network
  observation, and deterministic assertions:

```yaml
name: Electron app via CDP bridge
steps:
  - browser.open:
      connect: http://127.0.0.1:9222
      url: app://index.html
  - browser.click:
      testid: save-btn
```

### `log.file`

Reports the text a log file gained while the step ran.

| Argument | Meaning |
| --- | --- |
| `path` | File to read. Required. Read as UTF-8; invalid bytes become replacement characters. |

The result is an object with `lines`, `text`, `truncated` and `limited`:

| Field | Meaning |
| --- | --- |
| `lines` | The new text split into lines. A trailing `\r` is dropped, so a CRLF log reads like an LF one; a final newline does not start an empty line; a last line without a newline still counts as a line. `contains` on this list requires one element to equal the expected value; it is not a substring match, so use `text` for a substring. |
| `text` | The new text exactly as read. |
| `truncated` | `true` when the file is shorter than it was when the step started — it was truncated or rotated — and the read therefore restarted at the beginning. |
| `limited` | `true` when the step added more than 1 MiB: only the first 1 MiB is reported, so one noisy step cannot fill a failure bundle or a terminal. |

```yaml
name: Worker logs the order
observe:
  worker_log:
    log.file:
      path: ${log_dir}/worker.log
steps:
  - cli.run:
      command: ./worker
      args: ["--once"]
    expect:
      - path: $.exit_code
        equals: 0
      - path: $.observed.worker_log.text
        contains: "order created"
      - path: $.observed.worker_log.truncated
        equals: false
```

- The read starts at the length the file had when the step started, so lines
  that were already there never appear.
- A file that does not exist is empty, not an error: a program that creates it
  during the step is exactly what this observer is for. The observer never
  creates or changes the file.
- A path that is not a regular file — a directory, for example — and a file
  that cannot be read are environment errors (exit 4) that name the path and
  the reason. The check happens before the read, so the reason is the same on
  every platform.
- Rotation is detected by length. A file that is replaced between two steps by
  one of the same length or longer cannot be told apart from an append, and a
  file that is rewritten between two steps is read from its recorded length.

## Failure bundles

Terminal output is truncated and gone after the run, so every case that does not
pass also leaves a directory that an agent can read afterwards:

```text
.da/failures/
└── 1763000000123-4242/                run id: Unix milliseconds and process id
    ├── 0-wrong-exit-code/
    │   ├── case.yaml                  copy of the case file
    │   ├── report.json                the case as `--output json` shows it
    │   ├── browser/                   present only when a browser session had evidence
    │   │   ├── screenshot.png         the page, with editable fields covered
    │   │   └── dom.html               serialized page HTML, text-redacted
    │   ├── desktop/                   present only when a desktop session had evidence
    │   │   ├── screenshot.png         the window, with Edit/IsPassword controls blacked out
    │   │   └── uia-tree.json          serialized UIA tree, text-redacted
    │   └── meta.json                  written last
    └── 1-login-returns-a-token/
        └── (the same files; a case without a browser session has no browser/)
```

A session supplies failure evidence only after the case did not pass and only when
failure bundles are enabled. The runner asks before it closes the session, so the
page still exists; `--no-bundle` and a passing case never take a screenshot. The
evidence directory is named after the session kind, which keeps two sessions in
one case from colliding. Text files are redacted with `${env:NAME}` values; byte
files such as the PNG are written unchanged. A session that cannot collect its
evidence leaves `evidence-error.txt` in its directory with the reason, and the
case result and the rest of the bundle are unaffected.

`meta.json` holds at least these fields:

| Field | Meaning |
| --- | --- |
| `schema_version` | Version of the layout, currently `1`. |
| `da_version` | Version of `da` that wrote the bundle. |
| `case_file` | Case file, with `/` as the separator. |
| `case_sha256` | Lowercase hex SHA-256 of the source case file bytes. |
| `case_name` | Name of the case. |
| `project` | Project the case declares, or `null`. |
| `suite` | Suite the case declares, or `null`. |
| `layer` | Layer the case declares, or `null`. |
| `tags` | Tags the case declares, or `[]`. |
| `overridden_vars` | Sorted list of variable names supplied via `--vars-file` or `--var` (names only; never values). |
| `status` | `passed`, `failed` or `error`. A failure bundle contains only the last two; a run record also contains `passed`. |
| `created_at` | Unix milliseconds. |

`schema_version` stays `1`: the classification fields are an addition to the
layout, and a reader must ignore fields it does not know.

A case directory is staged under `.tmp-…` in the same parent, renamed into
place, and only then does `meta.json` appear. **A directory without `meta.json`
is incomplete and must be ignored**, including one left by an interrupted run.

The default directory is `.da/failures` relative to the working directory of
`da`. `--bundle-dir <directory>` replaces it and `--no-bundle` turns bundles off.
A run where every case passes creates nothing. When a bundle cannot be written,
the exit code of the run does not change: the problem is reported on standard
error and in the `bundle_errors` list of `--output json`.

> A bundle contains the arguments and results of every step. Values that came
> from `${env:NAME}` are replaced with `***`, but a credential that was written
> into a case file, a `--vars-file` or a `--var` value is not: treat a bundle as
> sensitive and keep its directory out of version control; `.da/` is in this
> repository's `.gitignore`.

## Run records and HTML reports

A failure bundle exists only after a case does not pass. To keep the same
evidence for **every** selected case, including the ones that pass, use
`--record`:

```sh
da run cases/ --record evidence/
da report evidence/
```

`--record <directory>` writes the same per-case layout as a failure bundle,
directly into the directory you name:

```text
evidence/
├── 0-login-returns-a-token/
│   ├── case.yaml
│   ├── report.json
│   ├── browser/
│   │   ├── screenshot.png
│   │   └── dom.html
│   └── meta.json                  written last
└── 1-a-case-that-failed/
    └── ...
```

The directory must be empty or absent. A run record is never mixed with an
older one, and `da` does not delete a directory it did not create. The runner
asks every open session for its evidence before closing it, so a passing case
now has a screenshot and DOM snapshot too. Without `--record`, a passing case
still takes no screenshot: the zero-overhead rule for ordinary runs is
unchanged.

To capture session evidence after **every executed step** (for example, one
masked screenshot and DOM snapshot per step during interactive UI flows), add
`--trace-steps` alongside `--record`:

```sh
da run cases/ --record evidence/ --trace-steps
```

`--trace-steps` requires `--record` (passing `--trace-steps` alone exits with
code 2). When enabled, each executed step writes non-empty session evidence
under `trace/steps/<i>/<kind>/` and each `finally` step writes under
`trace/finally/<i>/<kind>/` (where `<i>` is the 0-based index within that
block; unexecuted post-failure steps produce no trace directory). When
`--trace-steps` is omitted, per-step evidence collection is not invoked at all.
Any `local_only` constraint reported by a session also sets `"local_only": true`
in `meta.json`.

`--record` and failure bundles are independent. Failure bundles still go to
`.da/failures` (or `--bundle-dir`) unless `--no-bundle` is given. A failing case
can therefore appear in both places, written from the same collected evidence.
An error while writing either copy is listed in `bundle_errors`, printed on
standard error, and does not change the run's exit code.

`da report <directory>` reads only complete case directories, meaning a
directory whose `meta.json` exists. Incomplete staging directories are ignored
and the cases are ordered by the numeric prefix of their directory names. It
writes `<directory>/report.html` by default, or the file named by
`-o <file.html>`, and reports the path and counts on standard output. Use
`--output json` for one JSON object. A missing directory, or a directory with no
complete case, is a usage error (exit 2).

The report contains an overview, a step timeline for each case (including
collapsible resolved step arguments, saved variables, and any per-step artifacts
recorded under `trace/`), the assertions with their expected and actual values,
and side-by-side cards for the observed HTTP response, database rows, CLI
output, network requests, log lines, DOM snapshot and interface screenshot.
Both case-level screenshots and per-step traced screenshots obey `--shareable`,
`local_only`, and the 2 MiB / 10 MiB image budgets. An optional step that did not reach its
assertions appears as `SKIPPED` with the reason, is counted in the report's
skipped total, and does not stop the rest of the case from being recorded. It is
one static HTML file: CSS is inline,
there is no front-end framework, no script and no external reference. The
document carries this policy, so opening it from disk cannot fetch anything:

```html
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src 'unsafe-inline'">
```

Every value from a case, a step result, a DOM snapshot or an assertion is HTML
escaped, and URL schemes are entity-encoded in the text, so page content cannot
inject markup or start a request. The renderer reads the already-redacted
`report.json` and text evidence; it does not read `case.yaml` or expand
environment variables, so the recording step remains the single place that
redacts secrets.

Screenshots are embedded as `data:` URIs only when all of these hold:

- the report is not `--shareable`;
- the case's `meta.json` does not say `"local_only": true`;
- the image is at most **2 MiB**;
- the report's total embedded image size stays at or below **10 MiB**.

Otherwise the screenshot is not embedded and its card says exactly why (the
sharing mode, the `local_only` marker, the single-image limit or the total
budget). A `--shareable` report therefore contains no `data:image/` at all,
while it still keeps the DOM text, logs and results. Text evidence can still be
sensitive: review a report before publishing it, and use the recording
directory's `.gitignore` rules the same way you would for `.da/failures`.

## Output

`--output` is a global option, so it may stand before or after the subcommand:
`da --output json run cases/` and `da run cases/ --output json` are the same
command. The same holds for `lint` and `schema`.

`--output text` (the default) writes one line per case to standard output, lists
each failed assertion with its path, expected value and actual value, and ends
with a summary line:

```text
FAIL cases/login.yaml [Login returns a token] 6 ms
  http.request: 1 of 2 assertions failed
    $.status equals 201 (actual 200)
  bundle: .da/failures/1763000000123-4242/0-login-returns-a-token
Summary: 1 case (0 passed, 1 failed, 0 errors, 0 filtered), 2 steps (1 passed, 1 failed, 0 errors, 0 skipped), 7 ms
```

`--output json` writes exactly one JSON object to standard output:

```json
{
  "ok": false,
  "summary": {
    "cases": 1,
    "filtered": 0,
    "cases_passed": 0,
    "cases_failed": 1,
    "cases_errored": 0,
    "steps": 2,
    "steps_passed": 1,
    "steps_failed": 1,
    "steps_errored": 0,
    "steps_skipped": 0,
    "duration_ms": 7
  },
  "cases": [
    {
      "file": "cases/login.yaml",
      "name": "Login returns a token",
      "project": "shop",
      "suite": "smoke",
      "layer": "api",
      "tags": ["smoke", "fast"],
      "status": "failed",
      "duration_ms": 6,
      "bundle": ".da/failures/1763000000123-4242/0-login-returns-a-token",
      "error": { "kind": "assertion", "message": "1 of 2 assertions failed" },
      "steps": [
        {
          "name": "http.request",
          "status": "failed",
          "duration_ms": 6,
          "args": { "url": "http://127.0.0.1:8080/login", "method": "POST" },
          "result": { "status": 200, "headers": { "content-type": "application/json" }, "body": { "token": "t-123" } },
          "saved": {},
          "error": { "kind": "assertion", "message": "1 of 2 assertions failed" },
          "assertions": [
            { "path": "$.status", "comparison": "equals", "expected": 201, "actual": 200, "passed": false, "message": null },
            { "path": "$.body.token", "comparison": "exists", "expected": true, "actual": "t-123", "passed": true, "message": null }
          ]
        }
      ]
    }
  ],
  "errors": [],
  "bundle_errors": []
}
```

`bundle` is the directory of the case's failure bundle, or `null` when the case
passed or bundles are off. Every executed step report includes redacted resolved
`args` and redacted `saved` variables (`{}` when nothing was saved). When a case
declares `finally:`, its cleanup step
reports appear under `finally` on that case object (omitted when no `finally:`
block is present). `bundle_errors` lists the bundles that could not be
written; they never change the exit code. Every case also carries its
`project`, `suite`, `layer` (`null` when not declared) and `tags` (`[]` when
none), and `summary.filtered` counts the cases the filters excluded.

Every path that `da` prints, in text output, in JSON and in a failure bundle,
uses `/` as the separator on every platform, so a path from the output can be
copied back unchanged.

A value that `da` read through `${env:NAME}` appears as `***` in both formats
and in a bundle; see [Secrets](#secrets) for what that covers and where it
stops.

### Capturing JSON on Windows

`da` writes UTF-8 to standard output, and the shell can re-encode it on the way
to a file. Windows PowerShell 5.1, the `powershell.exe` that ships with Windows,
writes the result of `>` as UTF-16LE, so the file does not start with the `{` of
a JSON object and most tools refuse it:

```powershell
da --output json run cases/ > out.json   # out.json starts with FF FE
```

Capture the bytes `da` wrote with `cmd` instead, which keeps UTF-8 without a
byte order mark and keeps the exit code of the run:

```powershell
cmd /c "da --output json run cases/ > out.json"
```

`Set-Content -Encoding utf8` is no better in that version: it adds a byte order
mark (`EF BB BF`) at the start of the file. To check a captured file, look at
its first bytes; `7B`, the `{` of the object, is what a usable JSON file starts
with:

```powershell
Format-Hex -Path out.json | Select-Object -First 1
```

When no case can run, `cases` is empty and `errors` lists one entry per invalid
file, with `file`, `message` and `kind`. When the filters select no case,
`cases` and `errors` are empty, `ok` is `false`, `filter_error` holds the message
that names the filters and the available values, and the process exits 2.
Diagnostics never go to standard output.

## Exit codes

| Code | Meaning |
| ---: | --- |
| 0 | Every case ran and passed |
| 1 | At least one case failed an assertion or could not finish a step |
| 2 | Command-line usage error, including filters that select no case |
| 3 | Invalid case file, invalid step arguments, unknown step, unknown observer, invalid observer arguments, undefined variable or an unset `${env:NAME}` |
| 4 | Environment error (for example an unreachable target, or a log file that cannot be read) |
| 5 | Timeout |
| 9 | Reserved for a feature that is not implemented; no current command returns it |

When several kinds of problem occur in one run, the most fundamental one wins:
3, then 4, then 5, then 1.

`da report` returns 2 when the record directory is missing, is not a directory,
or contains no complete case record. It returns 4 when the HTML file cannot be
written.
