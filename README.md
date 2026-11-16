# AR Tracker

Weekly AR aging tracker for multiple operating companies. It is a Rust web server (axum) with a PostgreSQL database
and a browser UI.

## Run (local testing)

You need Rust (stable) and a PostgreSQL server. The defaults expect PostgreSQL on `localhost:5433`; point
`AR_DATABASE_URL` elsewhere if yours differs.

```
cargo run
```

Then open http://127.0.0.1:8080. If something else is already using 8080, set `AR_BIND=127.0.0.1:8090`.
Settings come from environment variables:

| Variable          | Default                                          | Purpose |
|-------------------|--------------------------------------------------|---------|
| `AR_DATABASE_URL` | `postgres://postgres@localhost:5433/ar_tracker`  | PostgreSQL connection. For a shared server, leave the password out and set `PGPASSWORD`; add `?sslmode=require` over a network |
| `AR_DB_SCHEMA`    | `ar`                                             | Schema for AR Tracker's tables (created if missing) |
| `AR_BIND`         | `127.0.0.1:8080`                                 | Address to listen on |

## Database

- PostgreSQL only: sqlx 0.8, with tables in their own schema, so it can share a server with other apps.
- Tables are created and upgraded automatically on start-up from `migrations/`.
- Native types: money is `NUMERIC(14,2)`, dates are `DATE`, timestamps are `TIMESTAMPTZ`, flags are `BOOLEAN`.
- `cargo test` needs the development server running (or `AR_TEST_DATABASE_URL`). Each test creates and drops its own
  temporary schema. The two tests that read a real aging report run only when `AR_SAMPLE_REPORT` points at one.

## My work (per collector)

**★ My work** (top of the sidebar) shows one account owner's open accounts across every company the viewer can see.
People whose sign-in is linked to an owner name land on it when they sign in.

- **Linking a person to their accounts:** an AR admin sets each user's **full name** on Users & access (for example
  "Cheryl Smith"). It must match the name on the company's account owner list. It's stored in `users.display_name`,
  and the change is logged.
- **Tiles:** open accounts, total balance, 90+ balance, follow-ups due, Top 25 behind on contact, no activity in
  7+ days, and two contact stats:
  - **Contact attempts last week:** calls, voicemails, emails, texts and site visits logged on the owner's accounts
    in the prior week (Monday–Sunday), by anyone.
  - **Avg contact attempts per day:** last week's attempts divided by 5 working days.
- **Contact methods used:** a donut chart of each contact method's share, with counts and percentages, hover details
  and a table view. The **Period** picker offers **Last 7 days** (the default), **Last 14 days** and **Last 30 days**,
  and the choice is remembered in that browser. The stats and the chart follow the company filter.
- **Sections, each account listed once, most urgent first:**
  1. **Follow-ups due:** the next follow-up date is today or earlier.
  2. **Top 25 accounts behind on contact:** a Top 25 account in its company (366-, 183-365, 91-182 or Total, the same
     rule as the grid highlight) with fewer than **2 contacts this week** (Monday–Sunday). Calls, voicemails, emails,
     texts and site visits count. The target is `TOP25_WEEKLY_CONTACTS` in `main.rs`.
  3. **No activity in 7+ days.**
  4. **All accounts**, collapsed.
- **Every row** has a quick-log box (type, note, Enter), and the customer name opens the full panel. The list refreshes
  after each entry, so an account leaves "behind on contact" as soon as it reaches the target.
- **Company filter:** for someone with accounts in more than one company, a **Company** dropdown (All companies, or one
  company, each with its account count) narrows the tiles and every list. The choice is remembered in that browser.
- **Other owners:** the **Account owner** picker shows any owner's list within the companies the viewer can see, for
  managers reviewing their team. Admins without a linked name start with the picker.

## Contacts

The customer panel starts with a **Contacts** section, so collectors see who to reach before logging anything.

- **AP Contact** is always listed first: name, phone, email and **preferred contact method**. There's one per customer,
  and the database enforces that. If it isn't set, the panel shows "AP Contact – not set" with an **Add AP contact**
  button.
- **Additional Contacts** is a separate heading under the AP Contact. **+ Add contact** there adds more contacts with
  the same fields. Instead of a fixed heading, each has a title box with the
  placeholder "Contact title" (for example Homeowner or Property Manager). A contact left untitled is called "Contact".
- **Preferred contact method** on every contact: Phone, Email, Text, Mail or Customer portal (`CONTACT_METHODS` in
  `main.rs`). It shows as a "Prefers …" tag, with a ★ next to the preferred phone or email. Phone numbers are
  click-to-call and emails are click-to-email.
- **Rules:** a contact needs at least a name, phone or email, and emails are checked for a valid format.
- **Who can edit:** anyone who can see the company. Additional contacts can be removed, but the AP Contact can only
  be edited.
- **My Work** shows each account's go-to contact under the customer name (the AP Contact, or else the first contact):
  title, name, preferred method, and a click-to-call or click-to-email link. Accounts without one show "No contact on file".
- **History:** every add, edit and removal goes in the permanent **Change history**, by database trigger, including the
  full details of removed contacts. Stored in `customer_contacts`.

## Promise to Pay

**Promise to Pay** and **Payment Plan** are separate Status choices. Promise to Pay can only be set by recording a
promise; the server refuses to set that Status any other way.

- **How to set it:** choose **Promise to Pay** in the grid's Status dropdown, or use **＄ Record promise to pay** in the
  customer panel. Either opens the promise form, which requires:
  - **Amount promised**
  - **Form of payment:** Check, ACH / bank transfer, Credit card, Cash, Customer portal or Other (`PAYMENT_METHODS`
    in `main.rs`)
  - **Expected payment date**
  - notes (optional)
- **Saving it:**
  - sets Status to Promise to Pay
  - adds a "Customer promised $X by date via method" activity entry
  - moves the next follow-up to the expected date, unless one is already set earlier
- **While it's open:** the account shows **"Promised $X by date"** in green in the grid and on My Work. It turns red,
  as **"Promise overdue"**, once the date passes. The promise can be edited, there's one open promise per account,
  and the Status dropdown can't change away from Promise to Pay until the promise is closed.
- **Closing it:** **✓ Kept**, **✗ Broken** or **Cancel promise**, with an optional note. Status goes back to what it was
  before (kept in `promises.status_before`), and an activity entry is added. Earlier promises stay listed in the panel.
- **History:** every change is in the permanent change history, and promises can't be deleted.

## Payment Plan

Payment Plan works the same way: it can only be set by recording a plan, never directly.

- **How to set it:** choose **Payment Plan** in the grid's Status dropdown, or use **📅 Set up payment plan** in the
  customer panel. The plan form requires:
  - **Amount agreed to** (pre-filled with the balance; it can be less)
  - **Each payment**, which can't exceed the amount agreed
  - **Frequency:** Weekly, Bi-weekly or Monthly
  - **Form of payment** (same list as promises)
  - **First payment date**, which sets the schedule
  - notes (optional)

  The form shows how many payments that makes and the size of a smaller last payment.
- **Saving it:** sets Status to Payment Plan, adds a "Payment plan agreed" activity entry, and moves the next
  follow-up to the first payment date unless one is already set earlier.
- **While it's open:** the grid and My Work show **"Plan $300/mo · next 12/1"**. Once the last scheduled date has
  passed, it turns red as **"Plan ended"** to prompt closing it. The schedule is by calendar
  (`plan_next_due` in migration 0011); it doesn't yet check receipts.
- **Limits:** one open plan per account, and an account can't have an open promise and an open plan at the same time.
- **Closing it:** **✓ Completed**, **✗ Defaulted** or **Cancel plan**, with an optional note. Status is restored, an
  activity is added, and plans are kept permanently in the change history.
- **Accounts already on Payment Plan:** accounts set to Payment Plan before this change keep that Status without plan
  details. Setting up a plan on them adds the details.

## Disputes

In the customer panel, **⚑ Mark as disputed** opens a dispute on the account. Anyone who can see the company can do this.

- **What's captured:**
  - **category**: Work not completed, Property or other damage, Change in scope, or Pricing dispute
  - disputed amount (defaults to the balance)
  - invoice or reference
  - **Resolution Owner**: the name of the employee following up on the dispute. It's free text, with suggestions
    from the company's account owner list. Shown as "not assigned" when blank, and editable while the dispute is open.
  - description (required)
  - who opened it and when
- **Status follows disputes:** opening a dispute sets the account's **Status** to **Disputed**, which is a Status choice
  with its own colour and appears in the status filter. When the **last** open dispute is resolved, Status goes back to
  what it was before (for example Payment Plan, or blank); that earlier value is kept in `disputes.status_before`.
  Reopening a dispute sets it to Disputed again. Every Status change is in the change history.
- **An account can have several disputes,** for example on different invoices. While any is open, the account shows
  **⚑ Disputed** in the grid and on My Work, and the grid has a **Disputed only** filter.
- **Disputes with no Resolution Owner are top priority** and are flagged until someone is assigned:
  - **Sidebar:** the company shows "⚠ N disputes need an owner" in red, on every page.
  - **Company grid:** a red banner ("N open disputes have no Resolution Owner") with a **Show them** filter, and the
    account's tag turns solid red: **⚑ Disputed · no owner**.
  - **My Work:** a **Disputes needing a Resolution Owner** card at the very top, plus an alert tile, with an **Assign**
    box on each row. It always covers all of the owner's companies, even when the company filter is set.
  - **Customer panel:** "Resolution Owner: not assigned" in red.
- **Open disputes can be edited.** **Resolve…** records an outcome (Credit or adjustment issued, Customer paid, Work
  completed / issue fixed, Written off, Withdrawn by customer, Other) and resolution notes, with who and when. AR
  admins can **Reopen** a resolved dispute.
- **History:** opening, resolving and reopening each add a **Dispute** entry to the activity log. Every change goes in
  the permanent **Change history**, by database trigger, and disputes can never be deleted.
- Categories and outcomes are `DISPUTE_CATEGORIES` / `DISPUTE_OUTCOMES` in `main.rs` (stored in table `disputes`).

## Dashboard (week over week)

**📊 Dashboard** (sidebar) shows all the companies the viewer can access combined, or one company. Pick it with **Show**,
or click a company in the table. The **Weeks** setting covers the last 8, 12, 26 or 52 weeks.

- **Focus week:** the latest week with an AR report, compared with the week before.
- **Tiles:**
  - Total AR, 90+ and % over 90, each with its change and an ▲/▼ arrow plus label
  - moved into 90+ this week (90+ dollars that weren't 90+ for that customer last week)
  - paid off / cleared (customers on last week's report but not this week's, and their balances)
  - new accounts on the report
  - collected
  - contacts logged
- **Charts:** AR by aging bucket (stacked, oldest darkest), % of AR over 90, collected per week and contacts per week.
  Each has hover tooltips, and there's a **Weekly numbers** table view.
- **Late reports:** in the all-companies view, a company without a report for a week is carried forward from its most
  recent report and flagged, so totals don't dip when one company is late.
- **Collected** comes from individual receipts, which the receipts import now stores in `receipts` (matched customers
  only; re-importing adds nothing). It covers customers tracked in AR Tracker, not total cash received.
- **Contacts** are calls, voicemails, emails, texts and site visits in the activity log.

## Company settings

Each company has its own settings page (**⚙ Company settings** in the company's toolbar). Only **AR admins** and that
company's **settings managers** can open or change it. Other staff assigned to the company don't see the button, and the
server refuses them (403).

- An AR admin makes someone a settings manager on **👥 Users & access** by ticking **⚙** next to a company. This is stored
  per company in `user_companies.can_manage_settings`. A manager can have settings access for some of their companies
  and not others, and it requires access to the company itself.
- Only AR admins can grant or remove it. Removing someone's access to a company also removes its settings access, and
  re-granting company access does not silently restore it. Every grant and removal is in **Access changes**.
- Staff still pick owners from the list on the company's accounts; only maintaining the list needs settings access.

Settings
are stored in `company_settings` (one JSON value per company and key), and every change is recorded in
`company_settings_log` with the old value, new value, who and when. The page shows the log under **Settings changes**.

**List of account owners** (`account_owners`) is the first setting:
- The **Account Owner** field on the company's accounts becomes a dropdown of these names. It can't be typed in. The
  server rejects any name not on the list and saves it with the list's spelling ("jordan lee" becomes "Jordan Lee").
- The page shows how many open and paid-off accounts each owner has.
- Names already on accounts that aren't on the list, from earlier typing or the Excel import, appear as "(not on list)"
  in the dropdown. The page offers a one-click **Add them to the list**.
- Removing an owner leaves their accounts with the old name, marked "(not on list)", until they're reassigned.
- If the Account Owner attribute is switched off for the company, the page says so. Switching it on is still done by
  an AR admin under ⚙ Attributes & companies.

## Edit history (permanent)

Recording happens inside PostgreSQL, using triggers, so every write path is covered: the UI, imports, and anything
run directly against the database. Each change is stamped with the signed-in user: the app sets `ar.user` on every
transaction.

- **Activity entries:** every version is kept in `activity_versions` (created, edited, deleted or restored, plus who
  and when). In the customer panel, an edited entry shows an "edited – history" link with every version.
- **Deleting is soft:** "Delete" hides the entry but keeps it. AR admins can tick **Show deleted** in the customer
  panel, see who deleted it, and **Restore** it. Staff can't see deleted entries or their history.
- **Field changes:** every change to an attribute value, the next follow-up date, and open/paid-off status goes in
  `change_log`, with the old value, new value, who and when. It's shown under **Change history** in the customer panel.
- **Tamper-resistant:** the database refuses to delete activity entries, and refuses any `UPDATE`, `DELETE` or
  `TRUNCATE` on the history tables and the access log.

## Sign-in and roles

Sign-in lives in `src/auth.rs`: a `TokenVerifier` trait, an `Authenticated` request extractor, and a dev verifier.

- **Debug builds** (`cargo run`) use **dev sign-in**: the sign-in page asks for a user name and role. Anyone who can
  reach the port can sign in as anyone, so keep `AR_BIND` on `127.0.0.1`.
- **Release builds** refuse to start until you add a production `TokenVerifier` for your identity provider (for
  example an OIDC / JWT verifier whose groups claim carries the roles below) and return it from `verifier()` in
  `main.rs`. Nothing else needs to change.

| Role (identity provider group) | Can do |
|---|---|
| `ar_admin` | Everything: import AR reports and Excel history, manage attributes, companies and per-company toggles, edit or delete anyone's activity entries (including imported Excel notes) |
| `ar_staff` | View all companies and accounts, log activity, set follow-ups and attribute values, edit or delete **their own** entries |
| anyone else | Signed-in users without either group (for example users of other apps on the same identity provider) get "no access" |

Activity authors, attribute "updated by" and import "imported by" come from the signed-in user, not from anything the
browser sends.

### Company-level access

Sign-in says who someone is and whether they are `ar_admin` or `ar_staff`. **Which companies** a staff user
can see is managed in the app, on **👥 Users & access** (admin only).

- Each user can be given **zero, one or many** companies. A user with none sees a "No companies assigned yet" page.
- `ar_admin` users always see every company.
- People appear on the page automatically the first time they sign in, marked "no access" until an admin ticks
  companies. Admins can also add someone ahead of time by their sign-in name. Removing a user removes all of their access.
- This is enforced on the server for every request: the company list, grids, import history, customer panels, activity
  entries, attribute edits and follow-ups. A customer or entry in a company the user can't see returns "not found",
  so IDs can't be probed.
- Every grant and removal is recorded under **Access changes** with who did it and when.

## Weekly workflow

1. Export the "Customer aging report" from the accounting system (one or more location sheets per file).
2. Click **Import AR report(s)** and select one or more files. Each sheet is matched to a company by its
   `Location:` code (for example `1001--Example Co` becomes company `1001`). New companies are created automatically.
3. For each company with balances on the report:
   - Balances are stored as a new weekly snapshot.
   - Customers no longer on the report move to **Paid off / archived**, and their notes and attributes are kept.
   - Archived customers that reappear are reactivated, with their history intact.
   - Re-importing the same as-of date replaces that snapshot. An older report than the latest is refused.
   - Sheets with no balances are skipped, so an empty sheet never archives everyone.

## Loading history from the existing Excel trackers

**Import notes history** reads a company's formatted tracking workbook. Import that company's latest AR report first.

- The company comes from the workbook's `Location:` line. Every sheet with a `Customer ID` header is read, including the
  "Notes not matched" and "No Matches Found" sheets. Invoice-detail sheets are skipped.
- Columns with "note" in the header become activity-log entries (type Note). The week comes from the header ("Week of 11/9",
  "Wk of 9/29", "9.17.2026 Meeting Notes", "wk of 31 Aug", …) and is moved to the Monday of that week. A notes header
  with no date, such as "Latest note", goes in the report's week, labelled with the column name.
- Other columns are matched to global attributes by name. Matching ignores wording such as "(Enter Date Sent)",
  "Actual" and "Customer". Unmatched columns that hold data become new global attributes. Matched attributes are
  switched on for that company.
- **Status** comes from the fill colour of the Customer ID cell, using the colour legend at the top of the sheet
  (for example a legend in O1:P4). Legend labels map to the existing Status choices. Unfamiliar ones are added
  as new choices with the same colour.
- Customers in the workbook who are not on any imported report go straight to the archive, with their notes.
- Notes and attribute values already in the app are never overwritten, so re-running the import is safe.

## Receipts register (last payment)

**Import receipts** (admin) reads the accounting system's receipts register as CSV, or the same report saved as Excel.
It updates each customer's **Last Known Payment Date** and **Last Known Payment Amount**.

- Rows are matched by the Customer ID before the `--` in the Customer column (for example `SL_12345--Jane Doe`).
  The file has no Location line, so the ID alone finds the customer and company. If an ID exists in two companies,
  it is skipped and listed.
- The amount comes from **Base amount**. For each customer, the most recent receipt date is used, and several receipts on
  that date are added together. A day that totals zero or less is ignored.
- Open and paid-off / archived customers are both updated. Customers who aren't in the app (no AR balance ever imported)
  are counted and ignored.
- A newer payment date already in the app, for example one typed in by hand, is never replaced. Re-importing the same file changes nothing.
- The values show "receipts import (user)" as who last changed them, and the two attributes are switched on for that company.

## Aging buckets

The report's `-0, 1-30, 31-60, 61-90, 91-120, 121-` columns are converted to
`-0, 1-30, 31-60, 61-90, 91-182, 183-365, 366-`. When a report column spans more than one display bucket
(for example `121-`), the amount goes into a bucket by its age from the GL posting date. This reproduced a
hand-formatted tracking sheet exactly in testing.

## Logging AR work (activity log)

This replaces the weekly notes columns in Excel. Every piece of collections work is its own entry, with a date,
who logged it (the initials in the top right), a type, and the note text.

- **Types:** Note, Call, Voicemail, Email, Text, Site visit, Promise to pay, Payment received, Dispute, Escalation.
  Call, Voicemail, Email, Text and Site visit also update **Most Recent Contact Date / Method** automatically.
- **From the grid:** each row has a quick-add box in the **Activity** column. Pick the type, type the note and press
  Enter. Below it you see this week's entries, and everything older collapses to one line ("▸ 12 earlier entries over
  6 weeks"). Click that line to expand it, grouped by week, or tick **Expand all history**.
- **Customer panel:** click a customer name. It has a fuller add form (pick the date, set the next follow-up) and the
  full history grouped by week. This week and last week start open, older weeks show a one-line preview, and you can
  search the history. Entries can be edited or deleted. Imported Excel notes are marked "import".
- **Next follow-up:** a date per customer, set from the panel or straight in the grid. Overdue dates turn red, and
  **Follow-up due** filters to them.
- **Last activity:** how long since the last entry. It turns amber at 7 days and red at 14 days or with none.
  **No activity in 7+ days** filters to those accounts, matching the "contact 2-3 times per week" expectation.
- Search in the toolbar also searches activity text.

## Attributes

**⚙ Attributes & companies** holds the master attribute list. Each attribute has a type (text, longtext, date, number,
money, or select with coloured choices), and the matrix turns it on or off per company. The attribute named **Status**
colours the customer name.

## Tests

`cargo test` runs a weekly rollover test (archive, refuse older reports, reactivate) against the sample report, if it is present.
