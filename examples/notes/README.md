# Team Notes example application

A deliberately small local application that an agent can explore with `da`:
one page with a form and a list, a JSON API over the same data, and one
PostgreSQL table. It is an example target of this package, so it needs nothing
beyond the Rust toolchain that builds `da`, the PostgreSQL server you point it
at and the browser a case drives.

## What it serves

| Route | Meaning |
| --- | --- |
| `GET /` | The page: a "New Note" form (Title, Content, Save), the saved notes with a "Pin" button each, and a "Display Preferences" card with a second Save button |
| `GET /api/health` | `{ "ok": true, "notes": <count> }` |
| `GET /api/notes` | `{ "notes": [ ... ] }`, newest first |
| `POST /api/notes` | `application/x-www-form-urlencoded` with `title` and `body`; answers `201 { "note": ... }`. A title that already exists updates that note instead of adding a second row, so a case can be replayed unchanged |
| `POST /api/notes/{id}/pin` | Toggles `pinned` and answers `200 { "note": ... }` |
| `DELETE /api/notes/{id}` | Deletes the row and answers `200 { "deleted": id }` |

Each note is one row of the `notes` table: `id bigint`, `title text unique`,
`body text`, `status text`, `pinned boolean`, `created_at timestamptz`. The app
creates the table on startup and inserts two seed rows when it is empty.

## Requirements

- The Rust toolchain that builds this repository (Rust 1.89 or newer).
- A PostgreSQL server (10 or newer) you can reach from this machine. The app
  speaks plain TCP without TLS, so use a URL with `sslmode=disable`; a local
  server satisfies that.
- For browser steps, a Chrome, Chromium or Edge that `da` can start; `DA_CHROME`
  names one explicitly if it is not in a standard location.

The app binds to loopback and is meant for local testing only.

## Start

```sh
# once: an empty database for the example
createdb notes

export DA_TEST_POSTGRES_URL='postgres://postgres@127.0.0.1:5432/notes?sslmode=disable'
cargo run --release --example notes
# notes: listening on http://127.0.0.1:3845 (database from DA_TEST_POSTGRES_URL)
```

The first build takes a while; later starts are immediate. `da` never starts,
restarts or stops the service it tests, so keep the app running in its own
terminal for as long as cases run against it.

| Variable | Required | Meaning |
| --- | --- | --- |
| `DA_TEST_POSTGRES_URL` | yes | `postgres://` or `postgresql://` URL of an existing database. A case reads the same variable (`url: ${env:DA_TEST_POSTGRES_URL}`), so `da` treats the string as a secret and redacts it from output and bundles. |
| `NOTES_PORT` | no | Port to listen on. Default `3845`. |
| `NOTES_HOST` | no | Address to bind. Default `127.0.0.1`. |
| `NOTES_RESET` | no | When set, the `notes` table is emptied before the seed rows are inserted. Use it to start from a known state. |

Check that it is up:

```sh
curl -s http://127.0.0.1:3845/api/health
curl -s -X POST --data 'title=Hello&body=from curl' http://127.0.0.1:3845/api/notes
```

## What it is for

The page is realistic enough to make an exploratory session work for the right
reasons:

- Title and Content are labelled and accessible as `textbox` + name, but carry
  no test id. The form's Save button and the preferences Save button share the
  accessible name `Save`, so a locator for one of them has to be narrowed with
  `within` or `nth` (or given a test id). Every note has a `Pin` button with the
  same name.
- Saving starts a request: the status line and the list change after the
  response arrives, so a step that asserts that text has to wait for it
  (`browser.wait`) rather than read the page immediately.
- The list is rendered from the API, which reads the same table a `db.query`
  step reads, so one case can assert the page, the API response and the row.

## Prepared breakage for the usage proof

`broken-db.patch` replaces the save path with one that answers `201` without
writing the row. The page still reports success, so a case fails only at the
step that checks the saved state: the database row or the API list, whichever
comes first.

```sh
git apply examples/notes/broken-db.patch
cargo run --release --example notes   # restart with NOTES_RESET=1 as well
# ... run the case; it must fail, and the bundle names the failing step
git apply -R examples/notes/broken-db.patch
```

Restart the app with `NOTES_RESET=1` (or delete the row first): a row left by an
earlier passing run would otherwise satisfy the database assertion, because the
broken app simply leaves the old row in place.

## Files

- `main.rs` — the app; run it with `cargo run --release --example notes`.
- `public/index.html` — the page.
- `broken-db.patch` — the prepared breakage described above.
