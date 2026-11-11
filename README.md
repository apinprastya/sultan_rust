# Sultan POS

A retail point of sale in Rust: one binary (`sultan`) for every node role (`standalone`, `hq`,
`branch`, `register`), a REST/JSON API under `/api/v1` with WebSocket event notifications, and
`SQLite` or `MySQL` storage. Design: `docs/plan.md`; agreed rules: `docs/spec/`.

## Quick start

Everything on a shop computer is done in windows; only an HQ server needs the command line.

1. **Open Sultan** (`sultan`, e.g. from a desktop shortcut; build it with
   `cargo build --release`, binary `target/release/sultan`). The first time, the **setup
   wizard** opens and asks how this computer is used:

   | App type | The wizard asks for | Afterwards, opening Sultan … |
   |---|---|---|
   | **Single app** | shop (company, branch code, time zone) and the Owner (password + PIN) | starts its own server on loopback and shows the **Start screen** |
   | **HQ server** | database (`SQLite` file or `MySQL` URL, with a connection test), listen address (default `0.0.0.0:8080`), shop, Owner | prints how to start it: `sultan serve` (see [Commands](#commands-for-scripts)) |
   | **Branch server** | – (shown, not available yet: it needs HQ↔branch sync) | – |
   | **Terminal** | the server address (e.g. `192.168.1.10`, port `8080` is added), then a manager logs in and names the register (e.g. `R2`, "Kasir 2"); or a pairing code | shows the **Start screen**, connected to that server |

2. The **Start screen** has three big tiles (keys or a tap):
   **F1 Cash register** (the lock screen: each cashier logs in with their PIN),
   **F2 Back office** (catalog: log in with a username and password) and
   **F3 Settings**. The footer says what the computer is connected to, e.g.
   "Connected to 192.168.1.10:8080 · R2 Kasir 2 · JKT01 · Toko Jakarta"; F5 checks again.
3. **F9** on the register's lock screen, or on the back office's login screen (after F12 log
   out), goes back to the Start screen; closing the register or back-office window does too.
   Closing the Start screen quits (a single app then stops its server). A single app's server
   keeps running while you switch between the register and the back office.
4. **Settings** (F3): the language (F6 switches Indonesian / English and saves it in
   `sultan.toml`; `SULTAN_LANG` overrides it), the connection (app type, server, register,
   branch, status), the version, and **F8 Set up again**, which opens the setup wizard (e.g. to
   make this computer a terminal of another server). The wizard never overwrites a database
   that is already set up; closing it returns to the Start screen. On a single app, Settings
   also has **Terminals in this shop** (see below).
5. **Add a terminal to a single app** (no file editing): on the single app, Start screen →
   **F3 Settings** → **F7** (or the switch) **Allow terminals to connect**. The app's server
   restarts on the shop network (port 8080) and Settings shows the address to type, e.g.
   `192.168.1.10:8080`. On the new computer, open Sultan, choose **Terminal** in the wizard, type
   that address, then log in as a manager (or use a pairing code). Terminals still need that
   login or code; turn this on only in a trusted shop network, and if a terminal cannot connect,
   allow incoming TCP port 8080 in the single app's firewall. F7 again turns it off (loopback
   only). If the port is taken by another program, Settings says so and keeps the previous
   setting; `SULTAN_LISTEN` fixes the address and locks the switch.

On a terminal whose server cannot be reached, the Start screen still opens and says so; F1 / F2
check again and show the problem instead of opening a window that cannot work. A register the
server no longer knows (unpaired) can still open the back office; set it up again from Settings.

Terminals register against an HQ server, or a single app that allows terminals (step 5; it is
saved as `listen = "0.0.0.0:8080"` in its `sultan.toml`; by default a single app listens on
loopback only). A branch server answers `read_only_replica`. The manager's login is used once and
not stored. A terminal stores its branch (`JKT01 · Toko Jakarta`) when it pairs, also with a
pairing code, and refreshes it each time the Start screen checks the connection.

![Start screen](docs/screenshots/0a-start-en-1366x768.png)

![Settings of a single app with terminals allowed](docs/screenshots/0d-settings-single-en-1366x768.png)

### Commands (for scripts)

An **HQ server** is run from the command line. On a single app or a terminal the commands are
for scripts only: everything they do is also reachable from the Start screen.

```sh
# HQ / scripts: first-run setup without the wizard. Without --owner-password / --owner-pin the
# password and PIN are asked for (hidden, twice).
sultan init --company "PT Sultan Demo" --branch JKT01 --branch-name "Toko Jakarta" --owner owner

sultan serve --role standalone --listen 0.0.0.0:8080   # Ctrl+C or SIGTERM stops it gracefully
sultan setup                     # the setup wizard again (also: Start screen → Settings → F8)

# A register machine without the wizard: pair with a one-time code issued in back office
# (POST /api/v1/registers/{id}/pairing-code), check, then open the window (without the Start
# screen).
sultan register pair --server http://<server-ip>:8080 --code ABCD-1234
sultan register status
sultan register                  # SULTAN_LANG=id or en to force the language

# Back office (catalog) without the Start screen, e.g. on the HQ server.
sultan backoffice                # server: --server, SULTAN_SERVER, the paired register's server,
                                 # else this machine's listen address (0.0.0.0 → 127.0.0.1)
sultan backoffice --server http://192.168.1.10:8080 --user owner
```

The back-office window manages the catalog (spec 2.2): products with their variants, units,
barcodes, bundle components, image and branch assortment, plus categories and brands. Editing
needs `catalog.manage` from any role assignment, for all branches or for one (the seeded Owner,
Manager and Stock clerk have it; catalog writes still happen only on the HQ or standalone
server); other users can browse read-only and the window says why. Archived products are listed
only with `catalog.view_archived`. A product, category or brand that was never used (e.g. a
typo) can be deleted (asking first); anything in use is refused with "archive it instead".
Changes made elsewhere arrive as `catalog.changed` events: the list refreshes and an open
product shows a warning with "reload".

The Start screen, the register and the back office are keyboard-first; every key is also a
button on screen:

| Start screen | Keys |
|---|---|
| Home | F1 cash register · F2 back office · F3 settings · F5 check the connection again |
| Settings | F6 switch the language (Indonesian / English) · F7 allow terminals to connect, on / off (single app only) · F8 set up again (Enter continue, Esc cancel) · Esc back |

| Register screen | Keys |
|---|---|
| Lock | ←/→/↑/↓ or Tab pick a user · a digit or Enter opens the PIN pad · digits, Backspace, Enter log in · Esc closes the pad · F5 reloads the users · F9 back to the Start screen |
| Open session | digits / Backspace type the opening cash (shown as Rupiah) · Enter opens · Esc locks. If another cashier's session is open here: Esc back to lock |
| Session home | type or scan into the barcode field (digits, letters, `-`, `.`; Backspace) · Enter looks the code up · Esc clears the field and the item · F2 test a manager override · F10 close the session · F12 lock (the session stays open) |
| Blind close | ↑/↓ or Tab move between the total and the denominations · digits / Backspace · Enter, then Enter again to confirm · Esc back |
| Close report | Enter: done (logs out to the lock screen) |
| Manager override | ↑/↓ pick the manager · digits / Backspace PIN · Enter approve (the action is retried with the token) · Esc cancel |

The back-office window is keyboard-first too; shortcuts work while a text field has the focus:

| Screen | Keys |
|---|---|
| Everywhere (after login) | F2 products · F3 categories · F4 brands · F12 log out |
| Login | Enter in the username moves to the password · Enter logs in · F9 back to the Start screen |
| Products | `/` or Ctrl+F search (typing filters after a short pause) · ↑/↓ move (past the last row loads more) · Enter open · Ctrl+N new product · Ctrl+Shift+D delete the highlighted product (never used only; asks first) · F5 refresh · Esc back from the search to the list |
| Product form | Ctrl+S save · Ctrl+1 / Ctrl+2 / Ctrl+3 tabs General / Variants / Units & barcodes · Esc back (asks first when there are unsaved changes) · Ctrl+Shift+A archive / unarchive (asks first) |
| Categories / Brands | ↑/↓ pick · Ctrl+S save · Ctrl+N add (a child of the picked category / a brand) · Ctrl+Shift+A archive / unarchive · Ctrl+Shift+D delete (never used only; asks first) · F5 refresh · Esc back to products |
| Confirmation | Enter or Y yes · Esc or N no |

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
| listen address | `--listen` | `SULTAN_LISTEN` | `listen` (single app: Start screen → Settings → Allow terminals) | `127.0.0.1:8080` (loopback only; a single app: a free loopback port) |
| node role | `--role` | `SULTAN_ROLE` | `role` | `standalone` |
| app type of a bare `sultan` (`single`, `server`, `client`) | – | – | `mode` | the setup wizard |
| Owner password (`init`) | `--owner-password` | `SULTAN_OWNER_PASSWORD` | – | hidden prompt |
| Owner PIN (`init`) | `--owner-pin` | `SULTAN_OWNER_PIN` | – | hidden prompt |
| log filter | – | `RUST_LOG` | – | `warn,sultan=info,sultan_app=info,sultan_server=info,tower_http=info` |
| window language (`id`, `en`) | – | `SULTAN_LANG` | `language` (Start screen → Settings) | system locale |
| back-office server (`sultan backoffice`) | `--server` | `SULTAN_SERVER` | `[register] server`, else `listen` | `http://127.0.0.1:8080` |

- One directory holds a node: `sultan.toml`, `sultan.db` and, on a register, `register.token`.
  The wizard writes `mode` (and `db`, `listen`, `role`, `[register]` as needed); a file without
  `mode` counts as `client` when it has `[register]` and as `server` when it has `db`.
  `init` writes `db` to `sultan.toml`; `register pair` writes a `[register]` table (server URL,
  register/branch IDs, code, name, and the branch label from the pairing answer). The Start
  screen's settings write `language` and, on a single app, `listen` (`0.0.0.0:<port>` when
  terminals are allowed; removed again, i.e. loopback, when not). Both files are written with mode `0600` on Unix, because
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
- The register window has no selling yet (spec 2.6): the session home looks up a scanned or
  typed barcode, scale label, SKU or PLU (`GET /catalog/lookup`) and shows the item, its unit,
  quantity and how it matched, or why it cannot be sold here; there is no cart and no price yet
  (spec 2.3), except the amount printed on a price scale label. The override test is still there. It needs a display (Slint picks Wayland, X11 or another backend; set
  `SLINT_BACKEND`, e.g. `winit-software`, to force one).
- The register does not reconnect the events WebSocket after a network drop; it reconnects at
  the next login. The back office behaves the same.
- Back office: the product image is picked by typing the file path (no file dialog yet); prices
  are not edited here (spec 2.3); categories, brands and products are archived, never deleted.

## Development

`make check` runs the gate: `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`
and `cargo llvm-cov --workspace --fail-under-lines 80`. `make test-mysql` runs the whole test
suite again on a throwaway `MySQL` 8.4 container (`crates/db/README.md`). API flows:
`postman/README.md`.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
