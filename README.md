# Sultan POS

A retail point of sale in Rust: one binary (`sultan`) for every node role (`standalone`, `hq`,
`branch`, `register`), a REST/JSON API under `/api/v1` with WebSocket event notifications, and
`SQLite` or `MySQL` storage. Design: `docs/plan.md`; agreed rules: `docs/spec/`.

## Quick start

```sh
cargo build --release            # binary: target/release/sultan
sultan                           # first start: the setup wizard opens (a window)
```

The wizard asks how this computer is used:

| App type | The wizard asks for | Afterwards, `sultan` … |
|---|---|---|
| **Single app** | shop (company, branch code, time zone) and the Owner (password + PIN) | starts its own server on loopback and opens the register at the lock screen |
| **HQ server** | database (`SQLite` file or `MySQL` URL, with a connection test), listen address (default `0.0.0.0:8080`), shop, Owner | prints how to start it: `sultan serve` |
| **Branch server** | – (shown, not available yet: it needs HQ↔branch sync) | – |
| **Terminal** | the server address (e.g. `192.168.1.10`, port `8080` is added), then a manager logs in and names the register (e.g. `R2`, "Kasir 2"); or a pairing code | opens the register window of this terminal |

Terminals register against an HQ server, or a single app that listens on the shop network
(`listen = "0.0.0.0:8080"` in its `sultan.toml`; by default a single app listens on loopback
only). A branch server answers `read_only_replica`. The manager's login is used once and not stored. `sultan setup` opens the
wizard again; it never overwrites a database that is already set up.

### Commands (for scripts)

```sh
# First-run setup without the wizard. Without --owner-password / --owner-pin the password and PIN
# are asked for (hidden, twice).
sultan init --company "PT Sultan Demo" --branch JKT01 --branch-name "Toko Jakarta" --owner owner

sultan serve --role standalone --listen 0.0.0.0:8080   # Ctrl+C or SIGTERM stops it gracefully

# On a register machine: pair with a one-time code issued in back office
# (POST /api/v1/registers/{id}/pairing-code), check, then open the window.
sultan register pair --server http://<server-ip>:8080 --code ABCD-1234
sultan register status
sultan register                  # SULTAN_LANG=id or en to force the language
```

The register window is keyboard-first; every key is also a button on screen:

| Screen | Keys |
|---|---|
| Lock | ←/→/↑/↓ or Tab pick a user · a digit or Enter opens the PIN pad · digits, Backspace, Enter log in · Esc closes the pad · F5 reloads the users |
| Open session | digits / Backspace type the opening cash (shown as Rupiah) · Enter opens · Esc locks. If another cashier's session is open here: Esc back to lock |
| Session home | F2 test a manager override · F10 close the session · F12 lock (the session stays open) |
| Blind close | ↑/↓ or Tab move between the total and the denominations · digits / Backspace · Enter, then Enter again to confirm · Esc back |
| Close report | Enter: done (logs out to the lock screen) |
| Manager override | ↑/↓ pick the manager · digits / Backspace PIN · Enter approve (the action is retried with the token) · Esc cancel |

Screenshots of every screen: `docs/screenshots/` (regenerate with
`cargo run -p sultan-ui --example screenshots -- docs/screenshots 1366x768 en`).

`init` refuses a database that is already set up; `serve` refuses one that is not.
`sultan --help` and `sultan <command> --help` list every flag.

### Configuration

Every setting is taken from, in order: **command-line flag > environment variable >
`sultan.toml` > default**.

| Setting | Flag | Environment | `sultan.toml` key | Default |
|---|---|---|---|---|
| config file | `--config` | `SULTAN_CONFIG` | – | `./sultan.toml` |
| database URL | `--db` | `SULTAN_DB` | `db` | `sqlite://<config dir>/sultan.db` (absolute path) |
| listen address | `--listen` | `SULTAN_LISTEN` | `listen` | `127.0.0.1:8080` (loopback only) |
| node role | `--role` | `SULTAN_ROLE` | `role` | `standalone` |
| app type of a bare `sultan` (`single`, `server`, `client`) | – | – | `mode` | the setup wizard |
| Owner password (`init`) | `--owner-password` | `SULTAN_OWNER_PASSWORD` | – | hidden prompt |
| Owner PIN (`init`) | `--owner-pin` | `SULTAN_OWNER_PIN` | – | hidden prompt |
| log filter | – | `RUST_LOG` | – | `warn,sultan=info,sultan_app=info,sultan_server=info,tower_http=info` |
| register window language (`id`, `en`) | – | `SULTAN_LANG` | – | system locale |

- One directory holds a node: `sultan.toml`, `sultan.db` and, on a register, `register.token`.
  The wizard writes `mode` (and `db`, `listen`, `role`, `[register]` as needed); a file without
  `mode` counts as `client` when it has `[register]` and as `server` when it has `db`.
  `init` writes `db` to `sultan.toml`; `register pair` writes a `[register]` table (server URL,
  register/branch IDs, code, name). Both files are written with mode `0600` on Unix, because
  `db` may contain a `MySQL` password.
- The `SQLite` database `sultan.db` and its `-wal` / `-shm` / `-journal` side files are owner-only
  (`0600`) on Unix: `init` creates them so, and `serve` (any open of the database) tightens
  existing files that others could read and logs a warning per file (fixed, not refused).
- `init --timezone` sets the first branch's IANA time zone (default `Asia/Jakarta`); document
  numbers such as the cashier session number `JKT01-R1-S-261108-01` use the branch-local date.
- The register's **device token** is kept out of `sultan.toml`, in `register.token` (mode `0600`).
- **Warning:** `--owner-password` / `--owner-pin` and the `SULTAN_OWNER_*` variables are for
  scripts only. Flags are visible in the process list and shell history; prefer the prompt.
- `MySQL`: pass a URL such as `--db mysql://user:pass@host/sultan` (the database must exist).
- Logs go to stderr; one line per request (method, path, status, latency). Header values,
  query strings and bodies are never logged.

### Limitations (current phase)

- `--role branch` answers master-data writes with `403 read_only_replica`, but HQ↔branch sync is
  not implemented yet: a branch node serves its local database only.
- The server speaks plain HTTP (no TLS yet); keep it on the shop LAN.
- The register window has no selling yet (spec 2.6): the session home is a placeholder with
  the override test. It needs a display (Slint picks Wayland, X11 or another backend; set
  `SLINT_BACKEND`, e.g. `winit-software`, to force one).
- The register does not reconnect the events WebSocket after a network drop; it reconnects at
  the next login.

## Development

`make check` runs the gate: `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`
and `cargo llvm-cov --workspace --fail-under-lines 80`. `make test-mysql` runs the whole test
suite again on a throwaway `MySQL` 8.4 container (`crates/db/README.md`). API flows:
`postman/README.md`.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
