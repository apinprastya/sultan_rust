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
   **F2 Back office** (catalog, customers and prices: log in with a username and password) and
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
| Open session | the opening cash starts at the float kept at this register's last close (the first digit typed replaces it) · digits / Backspace type the opening cash (shown as Rupiah) · Enter opens · Esc locks. If another cashier's session is open here: Esc back to lock |
| Sell screen (session home) | type or scan into the barcode field (digits, letters, `-`, `.`, `*`; Backspace) · Enter adds the item (`3*` before the code sets the quantity; an open-price item asks for its price) · Esc clears the field · F1 key list · F2 product search (type, Enter searches, ↑/↓ pick, Enter adds) · F3 customer · Shift+F3 customer by name (`customer.search`) · Ctrl+F3 detach the customer · Ctrl+T tax invoice on / off (a customer with a tax ID) · F4 pay, starting with cash · F9 pay with another tender · F5 open the drawer (`drawer.open`, audited; a manager approves for a cashier; the real kick comes with 2.11) · Shift+F5 cash in / out / safe drop (with or without a cart) · Shift+F12 X report (`session.view_expected`: a manager approves for a cashier; Esc back with the cart kept) · F7 price check on / off (a scan only shows the item and its prices) · Ctrl+↑ to the cart lines · Ctrl+D cart discount · Ctrl+N or Ctrl+Del void the cart (pick a reason) · F6 last receipts · F10 return / exchange (an empty cart; during an exchange F10 cancels it) · F11 hold the cart (with lines) · Shift+F11 resume a held cart · F12 lock (the session stays open; the cart waits in its crash copy) · Ctrl+F10 close the session (an empty cart only) |
| Cart lines (Ctrl+↑) | ↑/↓ pick a line (or a click) · Enter quantity · Del void the line (pick a reason) · `+` / `−` one more / less · U unit · P manual price · D line discount · Ctrl+D cart discount · Ctrl+N void the cart · Esc or Ctrl+↓ back to the barcode field. In a discount, `%` switches between percent and Rupiah; an empty value removes the discount |
| Payment (F4 / F9) | **Cash:** digits / Backspace type the cash received (Enter alone: exactly what is due, after cash rounding) · F9 another tender · Esc takes the last payment back (or closes the panel). **Other tender:** ↑/↓ pick (or a click) · Enter · type the amount (Enter alone: the rest, or the balance) · Enter · type the reference when the tender needs one · Enter · F4 cash. **Paid:** Enter confirms the sale · F8 save change to the customer's balance (type the amount, 0 gives it back) · Esc takes the last payment back. **Posted:** the change due and the receipt; Enter starts a new sale |
| Hold / resume (F11, Shift+F11) | **Hold:** type a note (optional) · Enter holds · Esc cancels. **Resume list:** ↑/↓ pick (or a click) · Enter resumes into the empty cart (with lines, Shift+F11 first asks to hold them) · Del deletes the held cart (`sale.void`, a manager approves) · Esc closes |
| Last receipts (F6) / purchase history | type or scan a receipt number (receipts only) · ↑/↓ pick (or a click) · Enter shows the receipt · F8 the next history page · Esc closes. **Receipt:** R reprint (COPY; any but the session's last sale needs `sale.reprint`) · V post-void · T return this sale · Esc back. **Post-void:** ↑/↓ pick a reason (type it for "other") · ← / → cash or the customer's balance (a sale with a customer) · Enter voids (`sale.post_void`) · Esc cancels |
| Return / exchange (F10) | **Which sale:** type or scan the receipt number, or ↑/↓ pick one of the attached customer's recent receipts · Enter · F2 without a receipt · Esc closes. **Lines:** ↑/↓ pick · Q or a digit types the quantity (by receipt; a decimal comma for weighed items, Enter sets it) · `+` / `−` one more / less · `*` all or none · Del write the line off (type the reason, Enter) · Tab cash or the customer's balance · Enter refunds · F9 exchange (the refund becomes the credit of a new cart; pay the difference as usual, or the payment shows what is paid back before Enter) · Esc back. Without a receipt: scan or type the items, Enter adds each; with the code field empty Q types the quantity of the picked item (3 decimals for a weighed item). **Done:** the cash to pay back and the return receipt; Enter closes |
| Customer (F3) | **Find:** type or scan a phone number or member code (`k7qh-29xm` works too; a scan here is always a member code, never a product) · Enter looks it up · Tab switches to a name search and back (only with `customer.search`) · ↑/↓ pick a result, Enter or a click opens it · F6 new customer (`customer.create`) · Esc closes. **Customer:** Enter attaches · F7 pay the balance out (`customer.balance_payout`, balance above 0) · F9 purchase history · Esc back. **New customer:** type the name, Tab (or ↑/↓, a click) to the phone (optional) · Enter signs up · F8 shows the customer who already has the phone · Esc back. **Payout:** digits / Backspace type the amount · Enter, then Enter again to pay out · the payout slip on screen: Enter or Esc back to the customer · Esc back |
| Cash in / out (Shift+F5) | ←/→ or I / O / D cash in, cash out, safe drop (I / O / D in the amount; a click on the tab) · ↑/↓ the reason (none for a drop) · digits / Backspace the amount · Tab to the note (type it; required for "Lainnya") and on to this session's movements · Enter, then Enter again to post (a cash out above the cashier's limit or a drop by a cashier: a manager approves) · the slip on screen: P reprint (COPY), F add a nota photo from a file, W from the webcam (only when one is found and the register is built with the `webcam` feature), Enter or Esc a new movement · **Movements:** ↑/↓ pick (or a click) · R reverse (type why, Enter; Esc back) · P reprint its slip · F / W add a nota photo to it (at most 3; the row shows the count and "No nota" for a cash out still missing one) · Tab back to the amount · Esc closes |
| X report (Shift+F12) | the session as of now with the expected cash (nothing is counted or closed) · Esc closes |
| Blind close | ↑/↓ or Tab move between the total and the denominations · digits / Backspace · Enter: keep as float (default the opening cash, at most the counted cash; digits / Backspace change it) · Enter again closes · Esc back to the count |
| Close report | the figures and the printable close report (float kept, to the safe) · Enter: done (logs out to the lock screen) · Z end of day (offered when no other session of the day is open, or to a cashier who may not check: then the run says which sessions are open) · Enter runs it (`day.close`, a manager approves for a cashier) and shows the Z · Esc back |
| Manager override | ↑/↓ pick the manager · digits / Backspace PIN · Enter approve (the void or the sale is retried with the token; a sale that needs several approvals asks for each in turn) · Esc cancel |

The back-office window is keyboard-first too; shortcuts work while a text field has the focus:

| Screen | Keys |
|---|---|
| Everywhere (after login) | F2 products · F3 categories · F4 brands · F6 customers · F7 pricing · F8 sales · F9 cash & Z · F12 log out |
| Login | Enter in the username moves to the password · Enter logs in · F9 back to the Start screen |
| Products | `/` or Ctrl+F search (typing filters after a short pause) · ↑/↓ move (past the last row loads more) · Enter open · Ctrl+N new product · Ctrl+Shift+D delete the highlighted product (never used only; asks first) · F5 refresh · Esc back from the search to the list |
| Product form | Ctrl+S save · Ctrl+1 / Ctrl+2 / Ctrl+3 / Ctrl+4 tabs General / Variants / Units & barcodes / Prices · Esc back (asks first when there are unsaved changes) · Ctrl+Shift+A archive / unarchive (asks first) |
| Prices tab | Ctrl+L next price list (or pick it) · ↑/↓ pick a price · Ctrl+N new price · Ctrl+Shift+N schedule a new price (starts tomorrow; saving ends the picked price the day before) · Ctrl+T add a tier (1–10) · Ctrl+Enter save the price · Ctrl+Shift+D delete it (asks first; only a price never sold) · needs `price.manage` to change |
| Categories / Brands | ↑/↓ pick · Ctrl+S save · Ctrl+N add (a child of the picked category / a brand) · Ctrl+Shift+A archive / unarchive · Ctrl+Shift+D delete (never used only; asks first) · F5 refresh · Esc back to products |
| Customer screens | Ctrl+1 customer list · Ctrl+2 groups · Ctrl+3 tiers · Ctrl+4 possible duplicates · Ctrl+5 negative balances · Ctrl+6 balance settings · F5 refresh |
| Customer list | `/` or Ctrl+F search name or company (needs `customer.search`) · ↑/↓ move (past the last row loads more) · Enter open · Ctrl+N new customer |
| Customer form | Ctrl+S save · Esc back (asks first when there are unsaved changes) · Ctrl+Shift+A archive / unarchive (asks first; `customer.manage`) · Ctrl+Shift+E erase personal data (`customer.erase`; type the member code, then Enter) · Ctrl+H purchase history |
| Purchase history | ↑/↓ move (past the last row loads more) · Ctrl+L load more · Enter open the sale or return · F5 refresh · Esc back to the customer |
| Sales screens | Ctrl+1 sales · Ctrl+2 returns · Ctrl+3 tender types · Ctrl+4 sales settings · F5 refresh · reading needs `report.sales` |
| Sales / Returns lists | filters branch, register, from / until (`YYYY-MM-DD`), customer (member code or phone), number, status (sales); Enter in a filter field or Search runs them, Clear resets · ↑/↓ move (past the last row loads more) · Ctrl+L load more · Ctrl+F to the number · Enter open |
| Sale / Return detail | ↑/↓ pick a linked document (the returns of a sale; a return's original and exchange sale) · Enter open it · Esc back (through the opened documents, then the list) |
| Tender types | ↑/↓ pick · Ctrl+N new · Ctrl+S save (the kind is fixed once saved) · Ctrl+Shift+A archive (asks first) · needs `tender.manage` at HQ |
| Sales settings | Ctrl+S save the return window (`return_days`, 0 = no limit) and the receipt width per branch (32 / 42 / 48) · needs `tender.manage` at HQ |
| Cash screens (F9) | Ctrl+1 sessions · Ctrl+2 cash movements · Ctrl+3 day close · Ctrl+4 cash reasons · F5 refresh · reading needs `report.sales` |
| Sessions | filters branch, register, cashier, from / until (`YYYY-MM-DD`, by open time), status; Search or Enter in a date runs them, Clear resets · ↑/↓ move (past the last row loads more) · Ctrl+L load more · Enter open |
| Session detail | an open session shows its live X (`report.sales` + `session.view_expected`; F5 reads it again), a closed one its close report · ↑/↓ its cash movements · Ctrl+P reprint the close report (COPY) · Esc back |
| Cash movements | filters branch, register, session, kind, reason, from / until · ↑/↓ move (past the last row loads more) · Ctrl+L load more · Enter shows the slip · Ctrl+P its copy (`sale.reprint`) |
| Day close | the branch picker (default your branch) · ↑/↓ through the days still to close, then the Z reports · Enter on a day runs its Z (asks first; `day.close`) · Enter on a Z shows it · Ctrl+P its copy · Ctrl+L more Z reports |
| Cash reasons | ↑/↓ pick · Ctrl+N new · Ctrl+S save (the direction is fixed once saved) · Ctrl+Shift+A archive (asks first) · needs `tender.manage` at HQ |
| Groups / Tiers | ↑/↓ pick · Ctrl+S save · Ctrl+N add · Ctrl+Shift+A archive / unarchive · Esc back to the customer list |
| Possible duplicates | ↑/↓ pick the customer to merge away (the one holding its phone is the one to keep; type another member code to change it) · Ctrl+M merge (asks first) · Enter open · Esc back |
| Negative balances | ↑/↓ move · Enter open · Esc back |
| Balance settings | Ctrl+S save · Esc back |
| Pricing screens | Ctrl+1 price lists · Ctrl+2 tax rates · Ctrl+3 pricing settings · F5 refresh · writes need `price.manage` |
| Price lists | ↑/↓ pick · Ctrl+N new list (a branch, customer group, membership tier or one customer by member code or phone; fixed once saved) · Ctrl+S save · Ctrl+Shift+A archive (asks first; not the default list) · Esc back to products |
| Tax rates | ↑/↓ pick the tax class · Ctrl+S add the rate from its first day (a future day is fine) · Esc back to price lists |
| Pricing settings | Ctrl+S save (switching tax-inclusive asks first: stored prices are not converted) · Esc back to price lists |
| Confirmation | Enter or Y yes · Esc or N no (the erase dialog: Enter and Esc only) |

Screenshots of every screen: `docs/screenshots/` (regenerate with
`TZ=Asia/Jakarta cargo run -p sultan-ui --example screenshots -- docs/screenshots 1366x768 en`).

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
- Sales (spec 2.6) are in the API (phase S3): tender types, `PUT /sales/{id}` (re-quoted and
  checked again at posting), receipts and reprints, post-voids, till voids (`POST /cart-voids`),
  returns and exchanges, held carts, the purchase history and the close report's sales figures;
  see `postman/README.md`. The return window `return_days` is fixed at 30 days (no setting yet),
  the receipt width is chosen per request (`?width=32|42|48`) and printing waits for 2.11.
- Cash management (spec 2.7) is in the API (phase K3) and at the register (K4): cash reasons,
  cash in / out / safe drop with reversals and slips (`/cash-movements`; Shift+F5), the payout
  slip, the X report (Shift+F12), the close report text with the float kept, the proposed float
  (`GET /registers/me/float`, the open screen starts there) and the Z report per branch and
  business day (`/z-reports`, run from the close report; days still to close from the day cash
  management was installed); see `postman/README.md`. The back office has F9 Cash & Z (K5):
  sessions with the live X or the close report, cash movements with their slips, the day close
  (days still to close, run the Z, the Z reports) and the cash reasons. Nota photos (K6): F / W in
  the cash dialog add up to three photos per movement (`/cash-movements/{id}/photos/{photo_id}`),
  stored as JPEG files in the node's data directory under `attachments/` (next to `sultan.db`,
  or next to `sultan.toml` with MySQL; back this directory up with the database), and a cash out
  of at least `nota_flag_above` (`/settings/sales`, default Rp 100 000) without one is flagged
  "missing nota". The file picker uses the XDG desktop portal on Linux (no portal: F shows the
  error); the webcam needs `cargo build -p sultan-app --features webcam`. The photo bytes are
  not pushed to HQ yet (no sync worker). The back office does not show or upload nota photos
  yet; slips and reports are text on screen until 2.11 prints them. The register offers
  the Z to a cashier without checking for other open sessions (a cashier may not list the days
  still to close); the run then names them.
- Inventory (spec 2.8) is in the API (phase N3) and the client, not yet on screen (N4–N6):
  suppliers and their items, stock reasons and the inventory settings (HQ data), purchase orders,
  goods receipts with the supplier's invoice, landed cost and a payable (a void needs a manager:
  `stock.adjust`, or `X-Override` at a register), supplier returns, transfers between branches on
  one database (request, ship, receive with a transit loss; `other_node` for a branch this node
  does not hold), internal transfers, adjustments, opening stock, opnames (several counters, one
  open count per location and item), reorder rules and suggested purchase orders, the on-hand
  list (filters negative, low, expiring, category, brand, text), the stock card, lots, payables
  and the till's on hand (`GET /registers/me/stock/{variant_id}`, device token, no cost); see
  `postman/README.md`. Without `report.profit`, cost and value fields are left out of the
  answers. Negative stock never blocks: postings answer `negative_after`. Payments to suppliers
  are 2.9.
- The register sells (spec 2.6 S4): scan, type or search (F2) items into the cart (a rescan adds
  to the line; weighed items, scale labels, open-price and manually priced lines always get a
  line of their own), change quantities and units, set manual prices and line or cart
  discounts, void lines or the whole cart with a reason (`POST /cart-voids`), and pay with split
  tenders (cash with change and cash rounding, cards, QRIS and others with a reference, the
  customer balance, saved change). Every change quotes the whole cart again
  (`POST /pricing/quote`); the server checks prices and permissions again at posting, and the
  approvals a cashier's limits do not cover (manual price, line and cart discount) are asked for
  when the sale is confirmed, one manager PIN per check. A `cart_stale` answer shows the new total
  and closes the payment panel. The cart is kept in a crash copy (`register.cart` next to
  `register.token`, owner-only) and comes back at the next login of the same cashier session.
  After the sale (S5): hold and resume (F11 / Shift+F11, held carts of the whole branch), the
  last receipts (F6) with reprints, post-voids (cash or the customer's balance) and returns,
  returns and exchanges (F10, by receipt or without one), the customer's purchase history (F3 →
  F9) and an audited drawer open (F5, `POST /drawer/open`). The receipt is shown on screen only
  (printing and the drawer kick wait for 2.11); the return-done panel shows the server's return
  receipt (`GET /returns/{id}/receipt`, S6); the receipt and history lists name the customer
  (another register's held cart shows the names it stored); a voided
  line is gone for good (no undo); a price-embedded scale label is quoted for its tax rate only;
  saved change is checked against the balance limits by the server at posting, not before.
  The F7 price check shows the item, its unit, quantity and how it matched, or why it cannot be
  sold here, with its store price and, with a customer attached, the customer's price and its
  source (spec 2.3 R5). The register needs a display (Slint picks Wayland, X11 or another
  backend; set `SLINT_BACKEND`, e.g. `winit-software`, to force one).
- The register does not reconnect the events WebSocket after a network drop; it reconnects at
  the next login. The back office behaves the same.
- Customers (spec 2.5) are in the API (`/customers`, `/customer-groups`, `/membership-tiers`,
  `/settings/customer-balance`). The back office (F6) has the customer list and form, groups,
  tiers, possible duplicates with merge, negative balances, erase and the balance settings; the
  list's "archived" switch works without a search only (the name search never finds archived
  customers). The register's customer step (F3) finds, signs up and attaches a customer and pays a
  balance out; the attached customer joins the sale (and is let go after it is posted, when the
  register locks, or with Ctrl+F3), and does not follow later edits made elsewhere until it is
  looked up again. The payout slip is shown on screen (printing waits for 2.11). Purchase history
  (`GET /customers/{id}/history`) lists the customer's sales and returns (spec 2.6 S3); the
  register shows it from S5. On a branch node only sign-up works; a till's
  profile edit forwarded to HQ (and `503 hq_unreachable`) comes with sync.
- Back office: the product image is picked by typing the file path (no file dialog yet);
  categories, brands and products are archived, never deleted. Pricing (F7, spec 2.3 R4): archived
  price lists are not listed and cannot be unarchived from the screen; a scheduled price change
  ends the old price with one call and creates the new one with a second (if the second is
  refused, the old end is put back); the Prices tab is not part of the product's unsaved-changes
  check, so an unsaved price is dropped without asking when you leave the product.

## Development

`make check` runs the gate: `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`
and `cargo llvm-cov --workspace --fail-under-lines 80`. `make test-mysql` runs the whole test
suite again on a throwaway `MySQL` 8.4 container (`crates/db/README.md`). API flows:
`postman/README.md`.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
