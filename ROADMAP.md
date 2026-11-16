# AR Tracker – improvement ideas

Ideas collected on 2026-11-15 from reviewing the formatted Excel trackers and notes. These are not commitments,
just a list to come back to. Items marked ✅ are done and 🚧 are in progress.

## Top recommendations

1. ✅ **My Work page for each collector (account owner)** – follow-ups due, owned accounts, and Top 25 accounts behind the
   "contact 2–3× per week until a promise to pay" expectation. *(Done 2026-11-15. Follow-ons: broken promises section
   once #2 exists, and an email digest.)*
2. 🚧 **Promises to pay and payment plans, checked against receipts** *(Promise to Pay status and form done 2026-11-15:
   amount, form of payment and expected date, closed as kept, broken or cancelled. Payment Plan status and form done
   the same day: amount agreed, each payment, frequency, form of payment and first date, with a calendar schedule. Still
   to do: automatically matching receipts to mark promises kept, flag broken promises and late plan payments.)* – a promise becomes a structured entry (amount and
   date) and a plan becomes amount, frequency and next due date. Receipts imports check them off automatically and flag
   broken promises and late plan payments.
3. ✅ **Week-over-week dashboards, including a roll-up of all companies** *(Done 2026-11-15; receipts are now stored for
   "collected".)* – total AR, % over 90, collected this week,
   newly over 90 and paid off, per company and across all 16–17, built from the weekly snapshots already stored.
4. **Collections and write-off approval workflow** – "Requesting write off" / "Requesting collections" becomes a request
   routed to an approver, with the activity history attached as backup and the decision recorded. Tracked through to
   "sent to agency" or "written off".
5. **Excel / PDF export in the current layout** – a per-company export matching today's formatted sheets, with the
   latest notes, for executives, auditors and the transition away from Excel.

## My Work additions

✅ Both done 2026-11-15.

- **Contact activity stats** – two tiles for the selected owner, following the company filter:
  - **Contact attempts last week:** total calls, voicemails, emails, texts and site visits logged on their accounts in
    the prior week (Monday–Sunday).
  - **Average contact attempts per day:** that total divided by the working days in the prior week (Monday–Friday
    assumed).
- **Contact method breakdown** – a pie chart of the share of each contact method (call, voicemail, email, text, site
  visit) on the selected owner's accounts, following the company filter. Each slice is labeled with its count and
  percentage, with a table view alongside. A period picker offers **Last 7 days** (default), **Last 14 days** and
  **Last 30 days**.

## Data worth capturing

- **Property management company** – grouping and totals for "c/o <management company>" names,
  since one call often covers several properties.
- **Billing portal and flags hidden in customer names** – detect billing-portal markers like `*PortalName*`, `*DO NOT SERVICE*`,
  `GIVE TO ACCTG` and `DO NOT USE THIS PROFILE`, and show them as tags (portal, service hold, duplicate profile).
- ✅ **Contacts for each customer** – an AP Contact plus additional contacts with their own title, each with name,
  phone, email and preferred contact method. *(Done 2026-11-15.)*
- ✅ **Disputes** – category (work not completed, property or other damage, change in scope, pricing), amount,
  reference, description, then outcome and resolution. *(Done 2026-11-15. Possible follow-ons: assign a resolver, an
  open-disputes tile on the dashboard, and leaving disputed accounts out of "behind on contact".)*
- **Attachments** – emails, certified letters and statements per account (S3 on AWS).

## Presentation

- **Saved views and choosing columns** – hide bucket columns or attributes, and save views such as "My 90+ accounts".
- **Compact rows** – a dense one-line-per-account mode.
- **Bulk actions** – assign an owner or set a status for several accounts at once.
- **Email digest** – a morning email of follow-ups due and broken promises.
- **Contact target per company** – make the Top 25 weekly contact target (currently 2) a company setting.

## Integrations

- **Sage Intacct REST API** – a scheduled sync of open invoices and receipts for every location, replacing weekly
  uploads. It gives invoice-level detail, exact due-date aging, and notes per invoice. Needs a Web Services license,
  a read-only API user, and ideally a sandbox. Credentials go in a secrets manager.
- **ServiceTitan** – show the service-hold status, or place the hold from AR Tracker.

## Before go-live (security)

- Production `TokenVerifier` in `src/auth.rs` (signature, expiry, issuer, client ID, token type, groups claim). MFA required.
- Use the identity provider's permanent `sub` ID as the user key instead of the user name.
- HTTPS only through Caddy, with the app listening only on the server itself.
- Content security policy and other security headers (move the page's script into its own file).
- Encrypted, versioned backups.
- Upload safety checks (zip-bomb detection, processing time limits).
- Dependency scanning (`cargo audit`).
