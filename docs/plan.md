> Carried over from a Claude Code session in /mnt/monday/project/qt/sultan (2026-11-07).
> The cashier shortcut spike (Rust + Slint 1.18.1, all 22 Qt shortcuts tested headlessly) lives at
> /mnt/monday/project/qt/sultan/sultan-spike (reference only; see its README for findings).
> Old Qt app for reference: /mnt/monday/project/qt/sultan

# Sultan POS: a new POS designed from scratch (Rust + Slint)

## Context
We are not porting the Qt app. We design a modern retail POS from first principles,
keeping only the agreed **architecture**: one Rust binary (standalone / server / client),
Slint native UI, REST/JSON API (+ WebSocket for event notifications only), SQLite + MySQL via `sqlx`, Indonesian + English,
≥80% test coverage. The cashier keyboard workflow is already proven (`sultan-spike/`).
Must-haves from the user: **multi-branch, product variants, price by quantity, price by
membership**. Everything else below is my proposal, to be accepted/rejected per item.

---

## 1. Architecture (agreed; extended for multi-branch)

**Node roles (same binary, chosen in setup wizard):**
- `hq`: head office. Owns master data (catalog, prices, promotions, customers, users).
  Aggregates all branch transactions for reporting.
- `branch`: branch server on the shop LAN. Serves its registers, holds a replica of
  master data and owns its own transactions + stock. **Keeps selling when internet/HQ is down.**
- `register`: till UI connecting to its branch server (`--connect`).
- `standalone`: hq + branch + register in one process (single shop; the common case).

**Sync model (no conflicts by design):**
- Master data flows HQ → branches; transactional data flows branch → HQ.
- Every write appends to an **outbox** table (same DB transaction). The sync worker uses REST:
  branch pushes with `POST /api/v1/sync/outbox` (batch, idempotent by event id) and pulls master
  data with `GET /api/v1/sync/changes?since=<cursor>`. A `sync.available` WebSocket event triggers
  a pull; a periodic pull is the fallback. Branch-level overrides (e.g. branch price list) are
  HQ-owned data scoped to a branch.
- IDs are **UUIDv7** (globally unique, time-ordered, safe to create offline).

**Protocol (decided 2026-11-07):**
- **REST/JSON over HTTP(S)** for all requests: `axum` server, `reqwest` client, versioned under `/api/v1`.
- Errors: `application/problem+json` (RFC 9457) with a stable `code` that the UI translates (ID/EN).
- Idempotent writes: IDs are created by the client (UUIDv7), so `PUT /resource/{id}` is safe to retry.
  Action `POST`s carry an `Idempotency-Key` header. Registers retry after LAN hiccups.
- **WebSocket only for event notifications** (`GET /api/v1/events`): small events such as `cart.held`,
  `price.changed`, `session.revoked`, `sync.available`. The client then fetches data over REST.
  No business writes go over WebSocket.
- OpenAPI generated from code (`utoipa`); API flows tested with Postman CLI collections.
- Register requests carry `X-Register-Token` (device token from pairing); users carry a bearer session
  token. See `docs/spec/2.1-organization-access.md`.

**Data principles:**
- Money = `i64` in minor units (Rupiah: 1). Quantities = fixed-point decimal (3 dp) for kg/liter.
- **Documents** (sale, purchase, transfer, opname...) have a lifecycle `draft → posted → voided`;
  posted documents are never edited, only reversed.
- **Ledgers are append-only**: stock movements, payments, customer points, store credit.
  Balances are derived and cached.
- Human document numbers per branch/register: `JKT01-R2-261107-0001`.
- Audit log for every sensitive change (price, discount, void, stock adjust, permission).
- Soft delete (archive) for master data; nothing referenced is ever hard-deleted.

**Cargo workspace:** `core` (pure domain: pricing, promotions, tax, stock, costing; no IO,
heavily unit-tested) · `db` (sqlx, migrations for SQLite + MySQL) · `server` (API, auth,
sync) · `client` (remote `Service` impl) · `hw` (ESC/POS, drawer, scale, display, labels) ·
`ui` (Slint) · `app` (binary, CLI, wizard) · `import` (old Sultan DB + XLSX/CSV).

---

## 2. Domain design

### 2.1 Organization & access
- Company → **Branches** → **Registers** (tills) and **Stock locations** (store floor,
  warehouse) per branch.
- **Roles** with fine-grained permissions (e.g. `sale.discount.max_10pct`, `sale.void`,
  `price.edit`, `stock.adjust`); users assigned roles **per branch**.
- Cashier login by **PIN** on the register; full password for back office.
- **Manager override**: cashier triggers an action beyond their permission → manager enters
  PIN on the spot; logged in audit.

### 2.2 Catalog
- **Product** (template: name, brand, category, tax class, type) → **Variants** (SKU) built
  from **option attributes** (Size: S/M/L × Color: Red/Blue; or Flavor for food).
  Products without options have a single default variant (no special case in code).
- **Units of measure per product** (shared by its variants) with conversion: `pcs`, `pack(6)`, `box(24)`. Each UoM can
  have **its own barcode and price**; stock is always kept in the base unit.
  (Replaces the old "box item link".)
- **Multiple barcodes** per variant/UoM (manufacturer + internal).
- Product types: `stock`, `service` (no stock), `bundle/kit` (fixed set of variants sold
  together, stock deducted from components), `composite/recipe` (ingredients, for F&B / repack),
  `weighed` (sold by kg via scale).
- **Scale barcodes**: EAN-13 with prefix 20–29 encoding PLU + weight or price (configurable format).
- Optional per product: **batch/expiry tracking** (FEFO), **serial number** tracking.
- Categories (tree), brands, tags, product images.
- **Auto barcode/SKU generator** (prefix + counter), **shelf label & barcode printing**.

### 2.3 Pricing engine (in `core`, pure + exhaustively tested)
- **Price lists**: `Default`, plus any number of lists scoped to **branch**, **membership
  tier**, **customer group** (wholesale), or a **single customer** (contract price).
  Each list entry: variant + UoM + **quantity tiers** (min qty → price) + valid from/to.
- An entry can be a **fixed price** or a **rule** (e.g. Gold = default −5%).
- **Resolution order** for a cart line (first match wins, then the lowest of matching tiers):
  1. manual price override (permission / manager override)
  2. customer contract price list
  3. customer's membership tier price list
  4. customer group price list
  5. branch price list
  6. default price list
  Within a list, the quantity tier with the highest `min_qty ≤ line qty` applies.
  *Option to discuss:* "best price for the customer" instead of first match.
- **Tax**: tax classes (PPN 11%/12%, exempt); prices tax-inclusive or exclusive per company setting.
- **Rounding**: line and total rounding rules; cash rounding (e.g. to Rp 100) with the
  difference recorded.

### 2.4 Promotions (applied after base price)
- Types: % / amount off (item, category, brand, whole cart), **buy X get Y**, **bundle price**
  ("3 for 10.000"), **mix & match**, spend threshold ("min 100k get 10k off"),
  **happy hour** (time/day schedule), **member-only**, **voucher/coupon codes** (single or bulk).
- Scope by branch, date range, membership tier; priority + **stacking rules**; per-customer usage limits.
- Receipt shows each applied promotion and "you saved Rp X".
- Manual discount at the till, capped by role.

### 2.5 Customers & membership
- Customer profile (phone as key; name, email, birthday, address), customer groups.
- **Membership tiers** (Silver/Gold/Platinum): linked price list, earning rate, benefits;
  **auto upgrade/downgrade** by rolling spend; member card / QR / phone lookup at the till.
- **Points**: earn rules (per Rp spent, per category, multipliers on promo days), redeem as
  payment or rewards, expiry. Append-only points ledger.
- **Store credit / receivable** (bon, piutang) with credit limit and payment collection.
- **Deposit / wallet** (customer prepaid balance).
- Purchase history per customer; birthday promotions.

### 2.6 Sales (register)
- Fast cart: scan / search / quick-key grid (touch) / scale; keyboard shortcuts (spike).
- **Hold & resume** carts, shared across registers in the branch.
- **Split payment** with multiple tenders: cash, debit/credit card (+ surcharge rules),
  **QRIS**, e-wallet, bank transfer, voucher, points, store credit, deposit.
- **Returns & exchanges** by receipt lookup (or no-receipt with permission), refund to
  original tender or store credit; restock or write-off.
- **Void** line / void sale with reason; post-void with manager override.
- **Down payment / layaway / customer orders** (pay later, pick up later).
- Receipt: print, reprint, **digital receipt** (WhatsApp/email link/QR) *(later)*.
- **Customer-facing display** (second screen as a second Slint window, or serial pole display).

### 2.7 Cash management
- **Cashier session** (one cashier + one register + one drawer; identity rules in
  `docs/spec/2.1-organization-access.md`): open with opening cash → sales → **cash in/out** (petty cash, with reason)
  → **blind close count** → over/short + close report. Handover = close + open a new session. **X report** (mid-session) and **Z report** (end of day).
- Cash drawer kick on cash tender; drawer-open log.

### 2.8 Inventory
- Stock per **location**, every change is a **stock movement** (sale, return, receipt,
  transfer out/in, adjustment, opname, production).
- **Costing: moving weighted average** (recommended; simple and auditable). FIFO optional later.
- **Purchasing**: purchase order → **goods receipt** (partial deliveries) → supplier invoice
  → **payable** with due date → payments. Supplier returns.
- **Inter-branch transfer**: request → ship (in transit) → receive (discrepancy recorded).
- **Stock opname** (full or cycle count, scanner-driven, freeze snapshot, variance posting).
- **Adjustments** with reasons (damaged, expired, lost, sample).
- **Reorder points** & low-stock alerts; **suggested purchase order** from sales velocity.
- **Expiry alerts** for batch-tracked items.

### 2.9 Money (accounting-lite)
- Payment accounts (each drawer, each bank, e-wallet settlement account).
- Expenses & other income with categories.
- Receivables (customers) & payables (suppliers) aging.
- Export journal to accounting software (CSV/XLSX) *(later: direct integrations)*.

### 2.10 Reports & dashboard
- Dashboard: today vs yesterday, per branch, top products, low stock, payables due.
- Sales by day / branch / register / cashier / product / category / brand / customer / tier.
- **Gross profit & margin**, inventory valuation, stock movement, slow/dead stock,
  promotion effectiveness, tax report, cashier session (X/Z) reports, membership & points liability.
- Every report filterable by branch + date; export XLSX/PDF.

### 2.11 Hardware
ESC/POS receipt printers (USB / serial / network / OS spooler) · cash drawer · barcode
scanner (keyboard wedge) · **scale** (serial) · customer display · **label printer**
(shelf labels/barcodes, e.g. TSPL/ZPL). Printing always happens on the node attached to the device.

### 2.12 Platform
Setup wizard · backup/restore + scheduled auto-backup · self-update of the single binary ·
data import (old Sultan DB one-time importer, XLSX/CSV for products/customers/stock) ·
audit log viewer · light/dark theme · i18n ID/EN.

---

## 3. Decisions (2026-11-07)
- **Multi-branch topology:** offline-first branch servers + HQ sync (section 1).
- **Price resolution:** fixed priority order (2.3); "lowest price wins" is not used.
- **Target businesses:** minimarket/grocery, fashion/general retail, wholesale/distributor.
  **F&B is out of scope**, so no table/order flow or modifiers. The `composite/recipe` product
  type is kept only as **repack** (e.g. 50 kg rice sack → 1 kg packs).
- Earlier decisions still stand: old-Sultan one-time importer, SQLite + MySQL, ID + EN.
- **Protocol:** REST/JSON for requests; WebSocket only for event notifications (replaces WebSocket+JSON).
- **2.1 Organization & access** (spec: `docs/spec/2.1-organization-access.md`):
  one company per install (for now) · every register is paired and identified by a device token, and
  every document records branch + register + user + cashier session · users and roles are edited
  **only at HQ** · selling requires an open **cashier session** (opening cash → blind close with
  report; handover = close and reopen) · register login = pick user + PIN.
- **2026-11-08 (2.1 follow-up):** document dates use the **branch's IANA time zone** (default
  `Asia/Jakarta`), e.g. cashier session `JKT01-R2-S-261107-01` with a per-register, per-local-day
  `NN` · the SQLite database file is owner-only (`0600`) · past sessions and their stored close
  reports are readable over the API (`GET /cashier-sessions[/{id}]`); reprint comes later and uses
  it · text compares byte-wise (case-sensitive) on SQLite and MySQL alike.
- **2026-11-08 2.2 Catalog** (spec: `docs/spec/2.2-catalog.md`): units of measure are defined
  **per product** (shared by its variants) · P1 types: stock, weighed + scale barcodes, service,
  bundle/kit · **max 3 option attributes** per product · **per-branch assortment** (HQ can block a
  product for a branch) · product codes and SKUs are stored upper case.
- **2026-11-09 UI only, except HQ:** shop users are not familiar with the command line. Only the
  **HQ server** may need it (`sultan serve`, scripts). On a single app, terminal or branch machine
  everything is reachable from a window: choosing register or back office, setup, pairing,
  language. CLI subcommands stay for scripts but are never the only way. Built as the **Start
  screen** a bare `sultan` opens (F1 register, F2 back office, F3 settings: language,
  connection, set up again, version; F9 on the lock / back-office login screen goes back). A
  single app opens to its terminals from Settings too ("Allow terminals to connect": restarts its
  server on the shop network and shows the address to type in the terminal's wizard).
- **2026-11-11 C3b decisions:** keep `GET /registers/me` (spec 2.1 §8) · a device token may open
  `/events` and gets only `Audience::Everyone` events, so the lock screen sees `catalog.changed`
  (spec 2.2 §17 item 3) · `catalog.manage` / `catalog.view_archived` count from **any** role
  assignment, not only an all-branches one (spec 2.2 §11, §17 item 4).

## Status (updated 2026-11-10)

Where we are, so any session can pick up from here. Update this section at the end of every phase.

**Done**
- Foundation: workspace (`core`, `db`, `server`, `client`, `ui`, `app`), CI, setup wizard
  (single app / HQ server / terminal), `sultan serve`, register pairing, SQLite + MySQL.
- **2.1 Organization & access** (spec `docs/spec/2.1-organization-access.md`): branches, registers
  with device tokens, users/roles/permissions, password + PIN login, cashier sessions (open, blind
  close, report), manager override, audit log, register lock screen and session home.
- **2.2 Catalog** (spec `docs/spec/2.2-catalog.md` §15), all phases C1–C5:
  C1 `core::catalog` · C2 `db` · C3 `server` API (PR #2) · C4 back-office window (PR #3) ·
  C5 register barcode lookup.
- **UI only, except HQ** (§3, 2026-11-09):
  - Start screen on single-app and terminal machines: F1 Kasir, F2 Back office, F3 Settings.
  - Settings: language (F6), set up again (F8), and on a single app "Allow terminals to connect"
    (F7), which restarts the built-in server on the shop network and shows the address to type.
  - Branch name on code-paired terminals: the pairing answer carries the branch;
    new `GET /registers/me` (device token) refreshes it for older pairings.

**Open decisions (the user's call)**
- None. The three earlier ones were decided on 2026-11-11 (see §3).

**Small to-dos**
- Settings on a single app: "Server address" shows `127.0.0.1:8080` even when terminals may connect;
  make clear it is this computer's own address (the terminal address is shown in the card beside it).
- Settings at 1024 px: the long status text ("Aplikasi tunggal: server berjalan di ko…") is cut off.

**Next (pick one)**
- **C3b**: the four items in spec 2.2 §17, all decided: delete never-used
  products/categories/brands, paging for search, device-token `/events` (Everyone events only),
  `catalog.manage` from any role assignment.
- **2.3 Pricing**: discuss the spec first (price lists, quantity tiers, membership, tax, rounding),
  then implement in phases like 2.2.

**Way of working**
- Each phase is discussed, written as a spec in `docs/spec/`, then built in phases; the user usually
  asks for each phase to run in a sub agent, and the result is re-checked (fmt, clippy, all tests,
  screenshots) before it is reported.
- Never commit or push; the user does that. New code ≥80% covered (`core` ≥95%).
- On a PR check, fix valid findings directly without asking first.
- Shop users never need the command line (only HQ may).

## 4. Release phases
| Phase | Content |
|---|---|
| **P1 Multi-branch core** | All node roles (hq/branch/register/standalone) + sync/outbox, inter-branch transfers, consolidated reports · users/roles/PIN/manager override · catalog with variants + UoM + multi-barcode + scale barcodes · pricing engine (qty tiers, membership, customer group/wholesale, branch, contract lists) + tax + rounding · customers + membership tiers · register (split tenders incl. QRIS, hold/resume, returns/exchanges, void) · cashier sessions X/Z + cash in/out · ESC/POS + drawer · stock movements + moving average, purchasing (PO → receipt → payable), opname, adjustments · receivables/payables (wholesale credit terms) · core reports + dashboard · old-Sultan importer. |
| **P2 Growth** | Promotions engine, points + rewards, store credit/deposit, scale device, label printing, reorder suggestions, batch/expiry, serial numbers, repack. |
| **P3 Extras** | Digital receipts, accounting export, self-update, customer display, layaway/customer orders, LAN auto-discovery. |

P1 is large. Inside P1 we build in this order, so there is always a runnable app:
standalone shop end-to-end → branch/register split → HQ + sync → transfers + consolidated reports.

## 5. How we discuss
Go section by section (2.1 → 2.12). For each: accept/reject sub-features, settle the rules
(edge cases written as test cases for `core`), and sketch the screen in Slint where useful.
Output: a spec file per section under `docs/spec/` in the new repo, and then implementation.

## 6. Verification
- `core`: table-driven unit tests for pricing resolution, promotions, tax, rounding,
  costing, stock movements (≥80% coverage, target ~95% for `core`).
- Headless Slint tests for register flows (pattern from `sultan-spike/tests/shortcuts.rs`).
- Server integration tests against SQLite **and** MySQL (CI matrix).
- API tests: Postman CLI collection (`postman/`) run against a live `sultan serve`.
- Sync tests: HQ + 2 branches in-process, branch offline then reconnect, assert convergence.
- Manual: standalone run; two-process branch + register on one machine.
