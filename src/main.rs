mod auth;
mod dashboard;
mod db;
mod history;
mod import;
mod receipts;

use axum::{
    extract::{DefaultBodyLimit, FromRef, Multipart, Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use chrono::{Datelike, Duration, Local, NaiveDate};
use auth::{Authenticated, Principal, Verifier};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type Db = PgPool;

/// Sign-in roles (identity provider groups in production).
mod roles {
    /// Imports reports and notes history, manages attributes, companies and user access; can edit anyone's entries.
    pub const AR_ADMIN: &str = "ar_admin";
    /// Views assigned companies, logs activity, sets follow-ups and attribute values; edits only their own entries.
    pub const AR_STAFF: &str = "ar_staff";
}

#[derive(Clone)]
struct AppState {
    db: Db,
    verifier: Verifier,
}
impl FromRef<AppState> for Db {
    fn from_ref(s: &AppState) -> Db { s.db.clone() }
}
impl FromRef<AppState> for Verifier {
    fn from_ref(s: &AppState) -> Verifier { s.verifier.clone() }
}

struct AppError(StatusCode, anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self { AppError(StatusCode::BAD_REQUEST, e.into()) }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": format!("{:#}", self.1)}))).into_response()
    }
}
type R = Result<Json<Value>, AppError>;

fn forbidden(msg: &str) -> AppError { AppError(StatusCode::FORBIDDEN, anyhow::anyhow!("{msg}")) }
fn not_found() -> AppError { AppError(StatusCode::NOT_FOUND, anyhow::anyhow!("not found")) }
fn is_admin(p: &Principal) -> bool { p.has_role(roles::AR_ADMIN) }
/// Any AR Tracker user. The identity provider may be shared with other apps, so signed in is not enough.
fn staff(p: &Principal) -> Result<(), AppError> {
    if p.has_any(&[roles::AR_ADMIN, roles::AR_STAFF]) { Ok(()) } else { Err(forbidden("you don't have access to AR Tracker")) }
}
fn admin(p: &Principal) -> Result<(), AppError> {
    if is_admin(p) { Ok(()) } else { Err(forbidden("only AR admins can do this")) }
}

// ---------------------------------------------------------------- company-level access
// Admins see every company. Staff see only the companies assigned to them on the
// Users & access page; with none assigned they see nothing.

/// Company ids this user may see; `None` means all (admin).
async fn allowed_companies(db: &Db, p: &Principal) -> anyhow::Result<Option<HashSet<i64>>> {
    if is_admin(p) { return Ok(None); }
    let ids: Vec<i64> = sqlx::query_scalar("SELECT company_id FROM user_companies WHERE username=$1").bind(&p.user).fetch_all(db).await?;
    Ok(Some(ids.into_iter().collect()))
}
async fn can_company(db: &Db, p: &Principal, company_id: i64) -> Result<(), AppError> {
    staff(p)?;
    match allowed_companies(db, p).await? {
        Some(set) if !set.contains(&company_id) => Err(forbidden("you don't have access to this company")),
        _ => Ok(()),
    }
}
/// A missing record and a record in someone else's company look the same (404), so ids can't be probed.
async fn can_customer(db: &Db, p: &Principal, customer_id: i64) -> Result<(), AppError> {
    staff(p)?;
    let co: i64 = sqlx::query_scalar("SELECT company_id FROM customers WHERE id=$1").bind(customer_id)
        .fetch_optional(db).await?.ok_or_else(not_found)?;
    can_company(db, p, co).await.map_err(|_| not_found())
}
async fn can_activity(db: &Db, p: &Principal, activity_id: i64) -> Result<(), AppError> {
    staff(p)?;
    let co: i64 = sqlx::query_scalar("SELECT cu.company_id FROM activities a JOIN customers cu ON cu.id=a.customer_id WHERE a.id=$1")
        .bind(activity_id).fetch_optional(db).await?.ok_or_else(not_found)?;
    can_company(db, p, co).await.map_err(|_| not_found())
}

/// Remember everyone who signs in, so admins can grant access without typing names.
async fn record_sign_in(db: &Db, p: &Principal) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO users (username, email, roles, first_seen, last_seen) VALUES ($1,$2,$3,now(),now())
                 ON CONFLICT (username) DO UPDATE SET email=COALESCE(excluded.email, users.email), roles=excluded.roles,
                 first_seen=COALESCE(users.first_seen, excluded.first_seen), last_seen=excluded.last_seen")
        .bind(&p.user).bind(&p.email).bind(p.roles.join(",")).execute(db).await?;
    Ok(())
}

#[cfg(debug_assertions)]
fn verifier() -> Verifier {
    eprintln!("ar_tracker: DEV SIGN-IN is on (debug build). Anyone who can reach this port can sign in as anyone.");
    Arc::new(auth::dev::DevVerifier)
}

#[cfg(not(debug_assertions))]
fn verifier() -> Verifier {
    eprintln!("ar_tracker: no production sign-in is configured yet (add a production TokenVerifier in src/auth.rs); run a debug build (`cargo run`) for local testing");
    std::process::exit(1)
}

async fn health(State(v): State<Verifier>, State(db): State<Db>) -> Json<Value> {
    let db_ok = sqlx::query("SELECT 1").execute(&db).await.is_ok();
    Json(json!({ "status": if db_ok { "ok" } else { "database unavailable" }, "sign_in": v.describe(), "dev_sign_in": cfg!(debug_assertions) }))
}

async fn me(State(db): State<Db>, Authenticated(p): Authenticated) -> R {
    let staff_ok = p.has_any(&[roles::AR_ADMIN, roles::AR_STAFF]);
    let mut companies = 0i64;
    if staff_ok {
        record_sign_in(&db, &p).await?;
        companies = match allowed_companies(&db, &p).await? {
            None => sqlx::query_scalar("SELECT COUNT(*) FROM companies").fetch_one(&db).await?,
            Some(s) => s.len() as i64,
        };
    }
    let display_name: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE username=$1").bind(&p.user).fetch_optional(&db).await?.flatten();
    Ok(Json(json!({ "principal": p, "admin": is_admin(&p), "staff": staff_ok, "companies": companies, "display_name": display_name })))
}

/// Body for `POST /auth/dev-login`. Roles default from the test user names `ar.admin` and `ar.staff`.
#[cfg(debug_assertions)]
#[derive(Deserialize)]
struct DevLogin {
    user: String,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    email: Option<String>,
}

#[cfg(debug_assertions)]
async fn dev_login(Json(b): Json<DevLogin>) -> R {
    let user = b.user.trim().to_string();
    if user.is_empty() { return Err(anyhow::anyhow!("user is required").into()); }
    let roles = if !b.roles.is_empty() { b.roles } else {
        match user.as_str() {
            "ar.admin" => vec![roles::AR_ADMIN.to_string()],
            "ar.staff" => vec![roles::AR_STAFF.to_string()],
            _ => vec![],
        }
    };
    let principal = Principal { user, email: b.email, roles };
    Ok(Json(json!({ "token": auth::dev::issue(&principal), "principal": principal })))
}

fn monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("AR_DATABASE_URL").unwrap_or_else(|_| "postgres://postgres@localhost:5433/ar_tracker".into());
    let schema = std::env::var("AR_DB_SCHEMA").unwrap_or_else(|_| "ar".into());
    let bind = std::env::var("AR_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let pool = db::connect(&url, &schema).await?;
    let state = AppState { db: pool, verifier: verifier() };

    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/health", get(health))
        .route("/api/me", get(me))
        .route("/api/companies", get(companies))
        .route("/api/companies/{id}", put(update_company))
        .route("/api/companies/{id}/grid", get(grid))
        .route("/api/companies/{id}/imports", get(imports))
        .route("/api/companies/{cid}/attributes/{aid}", put(toggle_attr))
        .route("/api/companies/{id}/settings", get(company_settings))
        .route("/api/companies/{id}/settings/account_owners", put(set_account_owners))
        .route("/api/customers/{id}", get(customer_detail))
        .route("/api/customers/{id}/attrs/{aid}", put(set_attr))
        .route("/api/customers/{id}/activities", post(add_activity))
        .route("/api/customers/{id}/follow_up", put(set_follow_up))
        .route("/api/activities/{id}", put(update_activity).delete(delete_activity))
        .route("/api/activities/{id}/versions", get(activity_versions))
        .route("/api/activities/{id}/restore", post(restore_activity))
        .route("/api/settings", get(settings))
        .route("/api/users", get(list_users).post(add_user))
        .route("/api/users/{username}", axum::routing::delete(remove_user))
        .route("/api/users/{username}/companies", put(set_user_companies))
        .route("/api/users/{username}/name", put(set_user_name))
        .route("/api/my-work", get(my_work))
        .route("/api/dashboard", get(dashboard_api))
        .route("/api/customers/{id}/disputes", post(open_dispute))
        .route("/api/customers/{id}/contacts", post(add_contact))
        .route("/api/customers/{id}/promises", post(open_promise))
        .route("/api/promises/{id}", put(edit_promise))
        .route("/api/promises/{id}/close", post(close_promise))
        .route("/api/customers/{id}/plans", post(open_plan))
        .route("/api/plans/{id}", put(edit_plan))
        .route("/api/plans/{id}/close", post(close_plan))
        .route("/api/contacts/{id}", put(update_contact).delete(delete_contact))
        .route("/api/disputes/{id}", put(edit_dispute))
        .route("/api/disputes/{id}/resolve", post(resolve_dispute))
        .route("/api/disputes/{id}/reopen", post(reopen_dispute))
        .route("/api/attributes", post(create_attr))
        .route("/api/attributes/{id}", put(update_attr))
        .route("/api/attributes/{id}/all", put(toggle_attr_all))
        .route("/api/import", post(do_import))
        .route("/api/import-history", post(do_import_history))
        .route("/api/import-receipts", post(do_import_receipts))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));
    #[cfg(debug_assertions)]
    let app = app.route("/auth/dev-login", post(dev_login));
    let app = app.with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("AR Tracker running at http://{bind}  (PostgreSQL schema '{schema}')");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------- companies & grid

async fn companies(State(db): State<Db>, Authenticated(p): Authenticated) -> R {
    staff(&p)?;
    let allowed = allowed_companies(&db, &p).await?;
    let rows: Vec<Value> = rows!(&db, "
        SELECT co.id, co.code, co.name, co.legal_name,
          (SELECT MAX(as_of) FROM imports WHERE company_id=co.id) AS as_of,
          (SELECT COUNT(*) FROM customers WHERE company_id=co.id AND status='active') AS active,
          (SELECT COUNT(*) FROM customers WHERE company_id=co.id AND status='archived') AS archived,
          (SELECT total FROM imports WHERE company_id=co.id ORDER BY as_of DESC LIMIT 1) AS total,
          (SELECT COUNT(*) FROM disputes d JOIN customers c2 ON c2.id=d.customer_id
            WHERE c2.company_id=co.id AND d.status='open' AND d.resolution_owner='') AS unassigned_disputes
        FROM companies co ORDER BY co.code")
        .into_iter()
        .filter(|r| allowed.as_ref().is_none_or(|s| r["id"].as_i64().is_some_and(|id| s.contains(&id))))
        .collect();
    Ok(Json(json!(rows)))
}

#[derive(Deserialize)]
struct CompanyUpd { name: String }
async fn update_company(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<CompanyUpd>) -> R {
    admin(&p)?;
    exec!(&db, "UPDATE companies SET name=$2 WHERE id=$1", id, b.name.trim());
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct GridQ { view: Option<String> }

async fn grid(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Query(q): Query<GridQ>) -> R {
    can_company(&db, &p, id).await?;
    let view = if q.view.as_deref() == Some("archived") { "archived" } else { "active" };
    let company = rows!(&db, "SELECT * FROM companies WHERE id=$1", id).pop().ok_or_else(not_found)?;
    let imps: Vec<(i64, NaiveDate)> = sqlx::query_as("SELECT id, as_of FROM imports WHERE company_id=$1 ORDER BY as_of DESC LIMIT 2")
        .bind(id).fetch_all(&db).await?;
    let prev_import = imps.get(1).map(|x| x.0).unwrap_or(-1);
    let attributes = rows!(&db, "
        SELECT a.id, a.name, a.kind, a.options FROM attributes a
        JOIN company_attributes ca ON ca.attribute_id=a.id AND ca.company_id=$1 AND ca.enabled
        WHERE NOT a.archived ORDER BY a.sort, a.id", id);

    let mut customers = rows!(&db, "
        SELECT cu.id, cu.ext_id, cu.name, cu.status, cu.first_seen, cu.last_seen, cu.archived_on, cu.follow_up,
               b.b0, b.b30, b.b60, b.b90, b.b182, b.b365, b.b366, b.total,
               (SELECT total FROM balances WHERE import_id=$3 AND customer_id=cu.id) AS prev_total,
               (SELECT string_agg(DISTINCT d.category, ', ') FROM disputes d WHERE d.customer_id=cu.id AND d.status='open') AS disputes,
               (SELECT SUM(d.amount) FROM disputes d WHERE d.customer_id=cu.id AND d.status='open') AS disputed_amount,
               (SELECT COUNT(*) FROM disputes d WHERE d.customer_id=cu.id AND d.status='open' AND d.resolution_owner='') AS dispute_unassigned,
               (SELECT json_build_object('id', p.id, 'amount', p.amount, 'expected_date', p.expected_date, 'method', p.payment_method)
                  FROM promises p WHERE p.customer_id=cu.id AND p.status='open') AS promise,
               (SELECT json_build_object('id', pl.id, 'total', pl.total_amount, 'each', pl.installment_amount, 'frequency', pl.frequency, 'method', pl.payment_method,
                       'next_due', plan_next_due(pl.first_due_date, pl.frequency, pl.total_amount, pl.installment_amount, $4),
                       'last_due', plan_due(pl.first_due_date, pl.frequency, plan_payments(pl.total_amount, pl.installment_amount) - 1))
                  FROM payment_plans pl WHERE pl.customer_id=cu.id AND pl.status='open') AS plan
        FROM customers cu
        LEFT JOIN balances b ON b.customer_id=cu.id AND b.import_id=(
            SELECT b2.import_id FROM balances b2 JOIN imports i ON i.id=b2.import_id
            WHERE b2.customer_id=cu.id ORDER BY i.as_of DESC LIMIT 1)
        WHERE cu.company_id=$1 AND cu.status=$2
        ORDER BY b.b366 DESC NULLS LAST, b.b365 DESC NULLS LAST, b.b182 DESC NULLS LAST, b.total DESC NULLS LAST, cu.name",
        id, view, prev_import, Local::now().date_naive());

    // Attribute values and activity for every customer in this view.
    let mut vals: HashMap<i64, serde_json::Map<String, Value>> = HashMap::new();
    for v in rows!(&db, "
        SELECT v.customer_id, v.attribute_id, v.value FROM attr_values v
        JOIN customers cu ON cu.id=v.customer_id WHERE cu.company_id=$1 AND cu.status=$2", id, view) {
        vals.entry(v["customer_id"].as_i64().unwrap_or_default()).or_default()
            .insert(v["attribute_id"].to_string(), v["value"].clone());
    }
    let mut notes: HashMap<i64, Vec<Value>> = HashMap::new();
    for n in rows!(&db, "
        SELECT a.id, a.customer_id, a.entry_date, a.kind, a.body, a.author, a.source, a.created_at, a.edited_at FROM activities a
        JOIN customers cu ON cu.id=a.customer_id WHERE cu.company_id=$1 AND cu.status=$2 AND a.deleted_at IS NULL
        ORDER BY a.entry_date DESC, a.created_at DESC, a.id DESC", id, view) {
        notes.entry(n["customer_id"].as_i64().unwrap_or_default()).or_default().push(n);
    }

    // "Top 25 in" flags (active view), matching the workbook highlight key.
    let mut top: HashMap<i64, Vec<&str>> = HashMap::new();
    if view == "active" {
        for (key, label) in [("b366", "366-"), ("b365", "183-365"), ("b182", "91-182"), ("total", "Total")] {
            let mut v: Vec<(i64, f64)> = customers.iter()
                .map(|x| (x["id"].as_i64().unwrap_or_default(), x[key].as_f64().unwrap_or(0.0)))
                .filter(|x| x.1 > 0.0).collect();
            v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (cid, _) in v.into_iter().take(25) {
                top.entry(cid).or_default().push(label);
            }
        }
    }
    for cu in customers.iter_mut() {
        let cid = cu["id"].as_i64().unwrap_or_default();
        cu["attrs"] = Value::Object(vals.remove(&cid).unwrap_or_default());
        cu["activities"] = json!(notes.remove(&cid).unwrap_or_default());
        cu["top25"] = json!(top.remove(&cid).unwrap_or_default());
    }

    Ok(Json(json!({
        "company": company,
        "view": view,
        "as_of": imps.first().map(|x| x.1.to_string()),
        "prev_as_of": imps.get(1).map(|x| x.1.to_string()),
        "current_week": monday(Local::now().date_naive()).to_string(),
        "today": Local::now().date_naive().to_string(),
        "attributes": attributes,
        "customers": customers,
        "account_owners": account_owners(&db, id).await?,
        "can_manage_settings": manages_settings(&db, &p, id).await?,
    })))
}

// ---------------------------------------------------------------- company settings
// Visible and editable by the company's assigned users and AR admins (the same check as
// the company's accounts). Every change is kept in company_settings_log.

/// Admins, or users an admin has made a settings manager for this company.
async fn manages_settings(db: &Db, p: &Principal, company_id: i64) -> anyhow::Result<bool> {
    if is_admin(p) { return Ok(true); }
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_companies WHERE username=$1 AND company_id=$2 AND can_manage_settings)")
        .bind(&p.user).bind(company_id).fetch_one(db).await?)
}
async fn can_manage_settings(db: &Db, p: &Principal, company_id: i64) -> Result<(), AppError> {
    can_company(db, p, company_id).await?;
    if manages_settings(db, p, company_id).await? { Ok(()) }
    else { Err(forbidden("only AR admins and this company's settings managers can open company settings")) }
}

const ACCOUNT_OWNERS: &str = "account_owners";
const ACCOUNT_OWNER_ATTR: &str = "Account Owner";

async fn account_owners<'e, E: sqlx::PgExecutor<'e>>(ex: E, company_id: i64) -> anyhow::Result<Vec<String>> {
    let v: Option<sqlx::types::Json<Vec<String>>> = sqlx::query_scalar("SELECT value FROM company_settings WHERE company_id=$1 AND key=$2")
        .bind(company_id).bind(ACCOUNT_OWNERS).fetch_optional(ex).await?;
    Ok(v.map(|j| j.0).unwrap_or_default())
}

async fn company_settings(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    can_manage_settings(&db, &p, id).await?;
    let company = rows!(&db, "SELECT id, code, name FROM companies WHERE id=$1", id).pop().ok_or_else(not_found)?;
    let owner_attr_on: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM company_attributes ca JOIN attributes a ON a.id=ca.attribute_id
        WHERE ca.company_id=$1 AND ca.enabled AND a.name=$2 AND NOT a.archived)").bind(id).bind(ACCOUNT_OWNER_ATTR).fetch_one(&db).await?;
    // Names currently set on this company's accounts, with how many open / paid-off accounts use each.
    let in_use = rows!(&db, "SELECT v.value AS name,
            COUNT(*) FILTER (WHERE cu.status='active') AS open, COUNT(*) FILTER (WHERE cu.status='archived') AS archived
        FROM attr_values v JOIN attributes a ON a.id=v.attribute_id JOIN customers cu ON cu.id=v.customer_id
        WHERE a.name=$2 AND cu.company_id=$1 GROUP BY v.value ORDER BY v.value", id, ACCOUNT_OWNER_ATTR);
    let log = rows!(&db, "SELECT key, old_value, new_value, changed_at, changed_by FROM company_settings_log
        WHERE company_id=$1 ORDER BY id DESC LIMIT 50", id);
    Ok(Json(json!({ "company": company, "account_owners": account_owners(&db, id).await?, "owners_in_use": in_use,
        "owner_attr_on": owner_attr_on, "log": log })))
}

#[derive(Deserialize)]
struct OwnersBody { owners: Vec<String> }

async fn set_account_owners(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<OwnersBody>) -> R {
    can_manage_settings(&db, &p, id).await?;
    // Tidy: trim, collapse spaces, drop blanks and case-insensitive duplicates (first spelling wins).
    let mut seen = HashSet::new();
    let mut owners = Vec::new();
    for n in b.owners {
        let n = n.split_whitespace().collect::<Vec<_>>().join(" ");
        if n.is_empty() { continue; }
        if n.chars().count() > 100 { return Err(anyhow::anyhow!("'{n}' is too long (100 characters max)").into()); }
        if seen.insert(n.to_lowercase()) { owners.push(n); }
    }
    if owners.len() > 500 { return Err(anyhow::anyhow!("too many names (500 max)").into()); }
    let mut tx = db::begin(&db, &p.user).await?;
    exec!(&mut *tx, "INSERT INTO company_settings (company_id, key, value, updated_by) VALUES ($1,$2,$3,$4)
        ON CONFLICT (company_id, key) DO UPDATE SET value=excluded.value, updated_by=excluded.updated_by, updated_at=now()",
        id, ACCOUNT_OWNERS, sqlx::types::Json(&owners), &p.user);
    tx.commit().await?;
    Ok(Json(json!({"ok": true, "account_owners": owners})))
}

async fn imports(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    can_company(&db, &p, id).await?;
    Ok(Json(json!(rows!(&db, "SELECT * FROM imports WHERE company_id=$1 ORDER BY as_of DESC", id))))
}

// ---------------------------------------------------------------- customer panel

async fn customer_detail(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    can_customer(&db, &p, id).await?;
    let cust = rows!(&db, "SELECT cu.*, co.code, co.name AS company FROM customers cu JOIN companies co ON co.id=cu.company_id WHERE cu.id=$1", id)
        .pop().ok_or_else(not_found)?;
    let history = rows!(&db, "
        SELECT i.as_of, b.b0, b.b30, b.b60, b.b90, b.b182, b.b365, b.b366, b.total
        FROM balances b JOIN imports i ON i.id=b.import_id WHERE b.customer_id=$1 ORDER BY i.as_of DESC", id);
    let lines = rows!(&db, "
        SELECT l.posting_date, l.source_bucket, l.bucket, l.amount FROM lines l
        WHERE l.customer_id=$1 AND l.import_id=(
            SELECT b.import_id FROM balances b JOIN imports i ON i.id=b.import_id
            WHERE b.customer_id=$1 ORDER BY i.as_of DESC LIMIT 1)
        ORDER BY l.posting_date", id);
    // Deleted entries are kept; only admins see them (to review or restore).
    let notes = rows!(&db, "SELECT a.id, a.entry_date, a.kind, a.body, a.author, a.source, a.created_at, a.edited_at, a.deleted_at, a.deleted_by,
            (SELECT COUNT(*) FROM activity_versions v WHERE v.activity_id=a.id) AS versions
        FROM activities a WHERE a.customer_id=$1 AND (a.deleted_at IS NULL OR $2)
        ORDER BY a.entry_date DESC, a.created_at DESC, a.id DESC", id, is_admin(&p));
    let attrs = rows!(&db, "
        SELECT a.name, v.value, v.updated_by, v.updated_at FROM attr_values v JOIN attributes a ON a.id=v.attribute_id
        WHERE v.customer_id=$1 ORDER BY a.sort", id);
    let changes = rows!(&db, "
        SELECT cl.changed_at, cl.changed_by, cl.field,
               CASE cl.field WHEN 'attribute' THEN COALESCE(a.name, 'attribute') WHEN 'follow_up' THEN 'Next follow-up' WHEN 'status' THEN 'Open / paid off' WHEN 'dispute' THEN 'Dispute' WHEN 'contact' THEN 'Contact' WHEN 'promise' THEN 'Promise to pay' WHEN 'plan' THEN 'Payment plan' ELSE cl.field END AS label,
               cl.old_value, cl.new_value
        FROM change_log cl LEFT JOIN attributes a ON a.id=cl.attribute_id
        WHERE cl.customer_id=$1 ORDER BY cl.id DESC LIMIT 300", id);
    let disputes = rows!(&db, "SELECT * FROM disputes WHERE customer_id=$1 ORDER BY (status='open') DESC, opened_on DESC, id DESC", id);
    Ok(Json(json!({"customer": cust, "history": history, "lines": lines, "activities": notes, "attrs": attrs, "changes": changes, "disputes": disputes,
        "dispute_categories": DISPUTE_CATEGORIES, "dispute_outcomes": DISPUTE_OUTCOMES,
        "contacts": rows!(&db, "SELECT id, is_ap, title, name, phone, email, preferred_method, updated_at, updated_by
            FROM customer_contacts WHERE customer_id=$1 ORDER BY is_ap DESC, id", id),
        "contact_methods": CONTACT_METHODS,
        "promises": rows!(&db, "SELECT * FROM promises WHERE customer_id=$1 ORDER BY (status='open') DESC, created_on DESC, id DESC", id),
        "payment_methods": PAYMENT_METHODS,
        "plans": rows!(&db, "SELECT pl.*, plan_payments(total_amount, installment_amount) AS payments,
                plan_due(first_due_date, frequency, plan_payments(total_amount, installment_amount) - 1) AS last_due_date,
                plan_next_due(first_due_date, frequency, total_amount, installment_amount, $2) AS next_due_date
            FROM payment_plans pl WHERE customer_id=$1 ORDER BY (status='open') DESC, created_on DESC, id DESC", id, Local::now().date_naive()),
        "plan_frequencies": PLAN_FREQUENCIES,
        "account_owners": account_owners(&db, cust["company_id"].as_i64().unwrap_or_default()).await?,
        "today": Local::now().date_naive().to_string()})))
}

#[derive(Deserialize)]
struct ValueBody { value: Option<String> }

async fn set_attr(State(db): State<Db>, Authenticated(p): Authenticated, Path((id, aid)): Path<(i64, i64)>, Json(b): Json<ValueBody>) -> R {
    can_customer(&db, &p, id).await?;
    let mut v = b.value.unwrap_or_default();
    // Account Owner must be one of the names in the company's settings list (stored with the list's spelling).
    let (attr_name, company_id): (String, i64) = sqlx::query_as("SELECT a.name, cu.company_id FROM attributes a, customers cu WHERE a.id=$1 AND cu.id=$2")
        .bind(aid).bind(id).fetch_optional(&db).await?.ok_or_else(not_found)?;
    // "Promise to Pay" and "Payment Plan" can only be set through their forms (they need the details).
    if attr_name == "Status" && v.trim().eq_ignore_ascii_case(PROMISE_STATUS) {
        return Err(anyhow::anyhow!("Promise to Pay needs the promise details – use the Promise to Pay form in the customer panel").into());
    }
    if attr_name == "Status" && v.trim().eq_ignore_ascii_case(PLAN_STATUS) {
        return Err(anyhow::anyhow!("Payment Plan needs the plan details – use the Payment Plan form in the customer panel").into());
    }
    // While a promise or plan is open the Status stays put; close it to change the Status.
    if attr_name == "Status" {
        match open_commitment(&db, id).await? {
            Some(Commitment::Promise) => return Err(anyhow::anyhow!("this account has an open promise to pay – mark it kept, broken or cancelled in the customer panel first").into()),
            Some(Commitment::Plan) => return Err(anyhow::anyhow!("this account has an open payment plan – mark it completed, defaulted or cancelled in the customer panel first").into()),
            None => {}
        }
    }
    if attr_name == ACCOUNT_OWNER_ATTR && !v.trim().is_empty() {
        let list = account_owners(&db, company_id).await?;
        if list.is_empty() {
            return Err(anyhow::anyhow!("this company has no account owner list yet – add the names under Company settings").into());
        }
        v = list.iter().find(|n| n.eq_ignore_ascii_case(v.trim())).cloned()
            .ok_or_else(|| anyhow::anyhow!("'{}' is not on this company's list of account owners", v.trim()))?;
    }
    let mut tx = db::begin(&db, &p.user).await?;
    if v.trim().is_empty() {
        exec!(&mut *tx, "DELETE FROM attr_values WHERE customer_id=$1 AND attribute_id=$2", id, aid);
    } else {
        exec!(&mut *tx, "INSERT INTO attr_values (customer_id, attribute_id, value, updated_by) VALUES ($1,$2,$3,$4)
            ON CONFLICT (customer_id, attribute_id) DO UPDATE SET value=excluded.value,
            updated_by=excluded.updated_by, updated_at=now()", id, aid, v.trim(), &p.user);
    }
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ActivityBody {
    entry_date: Option<String>,
    kind: Option<String>,
    body: Option<String>,
    /// None = leave the customer's follow-up alone; Some("") = clear it; Some(date) = set it.
    follow_up: Option<String>,
}

fn valid_date(s: &str) -> anyhow::Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| anyhow::anyhow!("invalid date '{s}'"))
}

/// Contact activities keep "Most Recent Contact Date / Method" current automatically.
async fn touch_contact(tx: &mut db::Tx, cid: i64, date: NaiveDate, kind: &str, user: &str) -> anyhow::Result<()> {
    if !db::CONTACT_KINDS.contains(&kind) { return Ok(()); }
    let id_of = |n: &'static str| sqlx::query_scalar::<_, i64>("SELECT id FROM attributes WHERE name=$1").bind(n);
    let d_id = id_of("Most Recent Contact Date").fetch_optional(&mut **tx).await?;
    let m_id = id_of("Most Recent Contact Method").fetch_optional(&mut **tx).await?;
    let Some(d_id) = d_id else { return Ok(()) };
    let date_s = date.to_string();
    let cur: Option<String> = sqlx::query_scalar("SELECT value FROM attr_values WHERE customer_id=$1 AND attribute_id=$2")
        .bind(cid).bind(d_id).fetch_optional(&mut **tx).await?;
    if cur.as_deref().is_none_or(|v| v <= date_s.as_str()) {
        for (aid, v) in [(Some(d_id), date_s.as_str()), (m_id, kind)] {
            let Some(aid) = aid else { continue };
            exec!(&mut **tx, "INSERT INTO attr_values (customer_id, attribute_id, value, updated_by) VALUES ($1,$2,$3,$4)
                ON CONFLICT (customer_id, attribute_id) DO UPDATE SET value=excluded.value,
                updated_by=excluded.updated_by, updated_at=now()", cid, aid, v, user);
        }
    }
    Ok(())
}

async fn add_activity(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<ActivityBody>) -> R {
    can_customer(&db, &p, id).await?;
    let body = b.body.unwrap_or_default();
    if body.trim().is_empty() { return Err(anyhow::anyhow!("note text is required").into()); }
    let date = match b.entry_date.as_deref() { Some(d) if !d.trim().is_empty() => valid_date(d)?, _ => Local::now().date_naive() };
    let kind = b.kind.filter(|k| !k.trim().is_empty()).unwrap_or_else(|| "Note".into());
    let mut tx = db::begin(&db, &p.user).await?;
    let aid: i64 = sqlx::query_scalar("INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1,$2,$3,$4,$5) RETURNING id")
        .bind(id).bind(date).bind(&kind).bind(body.trim()).bind(&p.user).fetch_one(&mut *tx).await?;
    touch_contact(&mut tx, id, date, &kind, &p.user).await?;
    if let Some(f) = b.follow_up {
        let f = if f.trim().is_empty() { None } else { Some(valid_date(&f)?) };
        exec!(&mut *tx, "UPDATE customers SET follow_up=$2 WHERE id=$1", id, f);
    }
    let row = rows!(&mut *tx, "SELECT id, customer_id, entry_date, kind, body, author, source, created_at FROM activities WHERE id=$1", aid);
    tx.commit().await?;
    Ok(Json(json!(row.into_iter().next())))
}

/// Staff may change only entries they wrote; admins may change any (including imported Excel notes).
async fn own_entry(db: &Db, p: &Principal, id: i64) -> Result<(), AppError> {
    let author: Option<Option<String>> = sqlx::query_scalar("SELECT author FROM activities WHERE id=$1 AND deleted_at IS NULL")
        .bind(id).fetch_optional(db).await?;
    let Some(author) = author else { return Err(not_found()) };
    if is_admin(p) || author.as_deref() == Some(p.user.as_str()) { Ok(()) } else { Err(forbidden("you can only change entries you logged")) }
}

async fn update_activity(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<ActivityBody>) -> R {
    can_activity(&db, &p, id).await?;
    own_entry(&db, &p, id).await?;
    if b.body.as_deref().is_some_and(|s| s.trim().is_empty()) { return Err(anyhow::anyhow!("note text is required").into()); }
    let date = b.entry_date.as_deref().map(valid_date).transpose()?;
    let mut tx = db::begin(&db, &p.user).await?;
    // One statement, so one edit = one saved version.
    exec!(&mut *tx, "UPDATE activities SET body=COALESCE($2, body), kind=COALESCE($3, kind), entry_date=COALESCE($4, entry_date), edited_at=now()
        WHERE id=$1 AND deleted_at IS NULL", id, b.body.as_deref().map(str::trim), b.kind.as_deref(), date);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

/// "Delete" only hides the entry: it stays in the database with who deleted it and when.
async fn delete_activity(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    can_activity(&db, &p, id).await?;
    own_entry(&db, &p, id).await?;
    let mut tx = db::begin(&db, &p.user).await?;
    exec!(&mut *tx, "UPDATE activities SET deleted_at=now(), deleted_by=$2 WHERE id=$1 AND deleted_at IS NULL", id, &p.user);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

async fn restore_activity(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    admin(&p)?;
    let mut tx = db::begin(&db, &p.user).await?;
    let n = exec!(&mut *tx, "UPDATE activities SET deleted_at=NULL, deleted_by=NULL WHERE id=$1 AND deleted_at IS NOT NULL", id);
    tx.commit().await?;
    if n == 0 { return Err(not_found()); }
    Ok(Json(json!({"ok": true})))
}

async fn activity_versions(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    can_activity(&db, &p, id).await?;
    let deleted: bool = sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM activities WHERE id=$1").bind(id).fetch_one(&db).await?;
    if deleted && !is_admin(&p) { return Err(not_found()); }
    Ok(Json(json!(rows!(&db, "SELECT change, entry_date, kind, body, changed_at, changed_by FROM activity_versions
        WHERE activity_id=$1 ORDER BY id", id))))
}

async fn set_follow_up(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<ValueBody>) -> R {
    can_customer(&db, &p, id).await?;
    let v = b.value.unwrap_or_default();
    let f = if v.trim().is_empty() { None } else { Some(valid_date(&v)?) };
    let mut tx = db::begin(&db, &p.user).await?;
    exec!(&mut *tx, "UPDATE customers SET follow_up=$2 WHERE id=$1", id, f);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------- attributes & settings (admin)

async fn settings(State(db): State<Db>, Authenticated(p): Authenticated) -> R {
    admin(&p)?;
    let attributes = rows!(&db, "SELECT * FROM attributes ORDER BY archived, sort, id");
    let companies = rows!(&db, "SELECT id, code, name FROM companies ORDER BY code");
    let enabled = rows!(&db, "SELECT company_id, attribute_id FROM company_attributes WHERE enabled");
    Ok(Json(json!({"attributes": attributes, "companies": companies, "enabled": enabled})))
}

#[derive(Deserialize)]
struct AttrBody {
    name: Option<String>,
    kind: Option<String>,
    options: Option<Value>,
    sort: Option<i32>,
    default_on: Option<bool>,
    archived: Option<bool>,
}
const KINDS: [&str; 6] = ["text", "longtext", "date", "number", "money", "select"];

async fn create_attr(State(db): State<Db>, Authenticated(p): Authenticated, Json(b): Json<AttrBody>) -> R {
    admin(&p)?;
    let name = b.name.unwrap_or_default();
    if name.trim().is_empty() { return Err(anyhow::anyhow!("name is required").into()); }
    let kind = b.kind.unwrap_or_else(|| "text".into());
    if !KINDS.contains(&kind.as_str()) { return Err(anyhow::anyhow!("invalid kind").into()); }
    let id: i64 = sqlx::query_scalar("INSERT INTO attributes (name, kind, options, sort, default_on)
            VALUES ($1,$2,$3,COALESCE($4,(SELECT COALESCE(MAX(sort),0)+10 FROM attributes)),$5) RETURNING id")
        .bind(name.trim()).bind(&kind).bind(b.options.unwrap_or(json!([])).to_string()).bind(b.sort).bind(b.default_on.unwrap_or(false))
        .fetch_one(&db).await?;
    Ok(Json(json!({"id": id})))
}

async fn update_attr(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<AttrBody>) -> R {
    admin(&p)?;
    if let Some(k) = &b.kind {
        if !KINDS.contains(&k.as_str()) { return Err(anyhow::anyhow!("invalid kind").into()); }
    }
    exec!(&db, "UPDATE attributes SET name=COALESCE($2,name), kind=COALESCE($3,kind), options=COALESCE($4,options),
        sort=COALESCE($5,sort), default_on=COALESCE($6,default_on), archived=COALESCE($7,archived) WHERE id=$1",
        id, b.name.as_deref().map(str::trim), b.kind.as_deref(), b.options.map(|o| o.to_string()), b.sort, b.default_on, b.archived);
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct Toggle { enabled: bool }

async fn toggle_attr(State(db): State<Db>, Authenticated(p): Authenticated, Path((cid, aid)): Path<(i64, i64)>, Json(b): Json<Toggle>) -> R {
    admin(&p)?;
    exec!(&db, "INSERT INTO company_attributes (company_id, attribute_id, enabled) VALUES ($1,$2,$3)
        ON CONFLICT (company_id, attribute_id) DO UPDATE SET enabled=excluded.enabled", cid, aid, b.enabled);
    Ok(Json(json!({"ok": true})))
}

async fn toggle_attr_all(State(db): State<Db>, Authenticated(p): Authenticated, Path(aid): Path<i64>, Json(b): Json<Toggle>) -> R {
    admin(&p)?;
    exec!(&db, "INSERT INTO company_attributes (company_id, attribute_id, enabled)
        SELECT id, $1, $2 FROM companies
        ON CONFLICT (company_id, attribute_id) DO UPDATE SET enabled=excluded.enabled", aid, b.enabled);
    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------- imports (admin)

async fn read_files(mut mp: Multipart, default_name: &str) -> Result<Vec<(String, Vec<u8>)>, AppError> {
    let mut files = Vec::new();
    while let Some(field) = mp.next_field().await? {
        if field.name() == Some("user") { continue; } // ignored: the signed-in user is recorded
        let name = field.file_name().unwrap_or(default_name).to_string();
        files.push((name, field.bytes().await?.to_vec()));
    }
    Ok(files)
}

async fn do_import(State(db): State<Db>, Authenticated(p): Authenticated, mp: Multipart) -> R {
    admin(&p)?;
    let mut results = Vec::new();
    for (name, bytes) in read_files(mp, "upload.xlsx").await? {
        let sheets = match import::parse_workbook(bytes) {
            Ok(s) => s,
            Err(e) => {
                results.push(json!({"file": name, "error": format!("{e:#}")}));
                continue;
            }
        };
        if sheets.is_empty() {
            results.push(json!({"file": name, "error": "No customer aging report sheets found (expected 'Location:' and a 'Customer ID' header)."}));
            continue;
        }
        for sd in &sheets {
            // One transaction per sheet: a refused or failed sheet leaves nothing behind.
            let mut tx = db::begin(&db, &p.user).await?;
            match import::apply_sheet(&mut tx, sd, &name, &p.user).await {
                Ok(r) => {
                    if r.status != "error" { tx.commit().await?; }
                    results.push(json!({"file": name, "result": r}));
                }
                Err(e) => results.push(json!({"file": name, "sheet": sd.sheet, "error": format!("{e:#}")})),
            }
        }
    }
    Ok(Json(json!(results)))
}

async fn do_import_history(State(db): State<Db>, Authenticated(p): Authenticated, mp: Multipart) -> R {
    admin(&p)?;
    let mut results = Vec::new();
    for (name, bytes) in read_files(mp, "upload.xlsx").await? {
        let mut tx = db::begin(&db, &p.user).await?;
        match history::import_history(&mut tx, bytes, &name, &p.user).await {
            Ok(r) => {
                tx.commit().await?;
                results.push(json!({"file": name, "result": r}));
            }
            Err(e) => results.push(json!({"file": name, "error": format!("{e:#}")})),
        }
    }
    Ok(Json(json!(results)))
}

async fn do_import_receipts(State(db): State<Db>, Authenticated(p): Authenticated, mp: Multipart) -> R {
    admin(&p)?;
    let mut results = Vec::new();
    for (name, bytes) in read_files(mp, "receipts.csv").await? {
        let parsed = match receipts::parse(bytes, &name) {
            Ok(rs) => rs,
            Err(e) => {
                results.push(json!({"file": name, "error": format!("{e:#}")}));
                continue;
            }
        };
        let mut tx = db::begin(&db, &p.user).await?;
        match receipts::apply(&mut tx, &parsed, &name).await {
            Ok(r) => {
                tx.commit().await?;
                results.push(json!({"file": name, "result": r}));
            }
            Err(e) => results.push(json!({"file": name, "error": format!("{e:#}")})),
        }
    }
    Ok(Json(json!(results)))
}

// ---------------------------------------------------------------- users & company access (admin)

async fn list_users(State(db): State<Db>, Authenticated(p): Authenticated) -> R {
    admin(&p)?;
    let mut users = rows!(&db, "SELECT username, email, roles, first_seen, last_seen, added_by, display_name FROM users ORDER BY lower(username)");
    let grants = rows!(&db, "SELECT username, company_id, can_manage_settings FROM user_companies");
    for u in users.iter_mut() {
        let name = u["username"].clone();
        let mine: Vec<&Value> = grants.iter().filter(|g| g["username"] == name).collect();
        u["company_ids"] = json!(mine.iter().map(|g| g["company_id"].clone()).collect::<Vec<_>>());
        u["settings_company_ids"] = json!(mine.iter().filter(|g| g["can_manage_settings"] == json!(true)).map(|g| g["company_id"].clone()).collect::<Vec<_>>());
    }
    let companies = rows!(&db, "SELECT id, code, name FROM companies ORDER BY code");
    let log = rows!(&db, "SELECT at, actor, username, change FROM access_log ORDER BY id DESC LIMIT 100");
    Ok(Json(json!({"users": users, "companies": companies, "log": log, "me": p.user})))
}

#[derive(Deserialize)]
struct NewUser { username: String, email: Option<String> }

async fn add_user(State(db): State<Db>, Authenticated(p): Authenticated, Json(b): Json<NewUser>) -> R {
    admin(&p)?;
    let name = b.username.trim();
    if name.is_empty() { return Err(anyhow::anyhow!("user name is required").into()); }
    let mut tx = db.begin().await?;
    let n = exec!(&mut *tx, "INSERT INTO users (username, email, added_by) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",
        name, b.email.as_deref().map(str::trim).filter(|e| !e.is_empty()), &p.user);
    if n == 0 { return Err(anyhow::anyhow!("{name} is already on the list").into()); }
    exec!(&mut *tx, "INSERT INTO access_log (actor, username, change) VALUES ($1,$2,'added user')", &p.user, name);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

async fn remove_user(State(db): State<Db>, Authenticated(p): Authenticated, Path(username): Path<String>) -> R {
    admin(&p)?;
    let mut tx = db.begin().await?;
    exec!(&mut *tx, "DELETE FROM users WHERE username=$1", &username); // cascades to user_companies
    exec!(&mut *tx, "INSERT INTO access_log (actor, username, change) VALUES ($1,$2,'removed user and all company access')", &p.user, &username);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct CompanyIds {
    company_ids: Vec<i64>,
    /// Companies whose settings this user may manage (must also be in company_ids). Omitted = unchanged.
    settings_company_ids: Option<Vec<i64>>,
}

/// Replace a user's company list (zero, one or many) and, optionally, which of those
/// companies they manage settings for. Admin only; every change goes in the access log.
async fn set_user_companies(State(db): State<Db>, Authenticated(p): Authenticated, Path(username): Path<String>, Json(b): Json<CompanyIds>) -> R {
    admin(&p)?;
    let mut tx = db.begin().await?;
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM users WHERE username=$1").bind(&username).fetch_optional(&mut *tx).await?;
    if exists.is_none() { return Err(AppError(StatusCode::NOT_FOUND, anyhow::anyhow!("unknown user {username}"))); }
    let current_rows: Vec<(i64, bool)> = sqlx::query_as("SELECT company_id, can_manage_settings FROM user_companies WHERE username=$1")
        .bind(&username).fetch_all(&mut *tx).await?;
    let current: HashSet<i64> = current_rows.iter().map(|r| r.0).collect();
    let current_mgr: HashSet<i64> = current_rows.iter().filter(|r| r.1).map(|r| r.0).collect();
    let wanted: HashSet<i64> = b.company_ids.into_iter().collect();
    let wanted_mgr: HashSet<i64> = match b.settings_company_ids {
        Some(v) => v.into_iter().collect(),
        None => current_mgr.intersection(&wanted).copied().collect(),
    };
    if let Some(id) = wanted_mgr.difference(&wanted).next() {
        return Err(anyhow::anyhow!("company {id}: settings access needs access to the company as well").into());
    }
    let label = |id: i64| sqlx::query_scalar::<_, String>("SELECT code || ' – ' || name FROM companies WHERE id=$1").bind(id);

    let mut changes: Vec<(i64, &str)> = wanted.difference(&current).map(|id| (*id, "granted")).collect();
    changes.extend(current.difference(&wanted).map(|id| (*id, "removed")));
    for (id, what) in changes {
        let Some(name) = label(id).fetch_optional(&mut *tx).await? else { return Err(anyhow::anyhow!("unknown company {id}").into()) };
        if what == "granted" {
            exec!(&mut *tx, "INSERT INTO user_companies (username, company_id, granted_by) VALUES ($1,$2,$3)", &username, id, &p.user);
        } else {
            exec!(&mut *tx, "DELETE FROM user_companies WHERE username=$1 AND company_id=$2", &username, id);
        }
        let note = if what == "removed" && current_mgr.contains(&id) { " (and its settings access)" } else { "" };
        exec!(&mut *tx, "INSERT INTO access_log (actor, username, change) VALUES ($1,$2,$3)", &p.user, &username, format!("{what} {name}{note}"));
    }
    // Settings-manager changes, for companies the user keeps (or just got) access to.
    for id in &wanted {
        let (was, now) = (current_mgr.contains(id), wanted_mgr.contains(id));
        if was == now { continue; }
        exec!(&mut *tx, "UPDATE user_companies SET can_manage_settings=$3 WHERE username=$1 AND company_id=$2", &username, id, now);
        let name = label(*id).fetch_one(&mut *tx).await?;
        let change = if now { format!("made settings manager for {name}") } else { format!("removed settings access for {name}") };
        exec!(&mut *tx, "INSERT INTO access_log (actor, username, change) VALUES ($1,$2,$3)", &p.user, &username, change);
    }
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct NameBody { display_name: Option<String> }

/// Link a sign-in to an account owner name (admin only, logged).
async fn set_user_name(State(db): State<Db>, Authenticated(p): Authenticated, Path(username): Path<String>, Json(b): Json<NameBody>) -> R {
    admin(&p)?;
    let name = b.display_name.map(|n| n.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|n| !n.is_empty());
    let mut tx = db.begin().await?;
    let old: Option<Option<String>> = sqlx::query_scalar("SELECT display_name FROM users WHERE username=$1").bind(&username).fetch_optional(&mut *tx).await?;
    let Some(old) = old else { return Err(AppError(StatusCode::NOT_FOUND, anyhow::anyhow!("unknown user {username}"))) };
    if old == name { return Ok(Json(json!({"ok": true}))); }
    exec!(&mut *tx, "UPDATE users SET display_name=$2 WHERE username=$1", &username, &name);
    exec!(&mut *tx, "INSERT INTO access_log (actor, username, change) VALUES ($1,$2,$3)", &p.user, &username,
        format!("full name: {} → {}", old.as_deref().unwrap_or("(none)"), name.as_deref().unwrap_or("(none)")));
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------- My Work (per account owner)

/// Weekly contacts expected for a Top 25 account ("2–3 times per week until a promise to pay").
const TOP25_WEEKLY_CONTACTS: i64 = 2;

#[derive(Deserialize)]
struct MyWorkQ { owner: Option<String> }

/// One owner's open accounts across every company the viewer can see, with what needs doing.
async fn my_work(State(db): State<Db>, Authenticated(p): Authenticated, Query(q): Query<MyWorkQ>) -> R {
    staff(&p)?;
    let allowed: Option<Vec<i64>> = allowed_companies(&db, &p).await?.map(|s| s.into_iter().collect());
    let me_name: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE username=$1").bind(&p.user).fetch_optional(&db).await?.flatten();
    let owner_attr: Option<i64> = sqlx::query_scalar("SELECT id FROM attributes WHERE name=$1").bind(ACCOUNT_OWNER_ATTR).fetch_optional(&db).await?;
    let status_attr: Option<i64> = sqlx::query_scalar("SELECT id FROM attributes WHERE name='Status'").fetch_optional(&db).await?;
    let today = Local::now().date_naive();
    let week_start = monday(today);

    // Owner names to choose from: the account owner lists plus names set on accounts, in companies the viewer can see.
    let owners: Vec<String> = sqlx::query_scalar("
        SELECT DISTINCT ON (lower(n)) n FROM (
            SELECT jsonb_array_elements_text(s.value) AS n FROM company_settings s
              WHERE s.key='account_owners' AND ($1::bigint[] IS NULL OR s.company_id = ANY($1))
            UNION ALL
            SELECT trim(v.value) FROM attr_values v JOIN customers cu ON cu.id=v.customer_id
              WHERE v.attribute_id=$2 AND cu.status='active' AND ($1::bigint[] IS NULL OR cu.company_id = ANY($1))
        ) x WHERE n <> '' ORDER BY lower(n), n")
        .bind(&allowed).bind(owner_attr).fetch_all(&db).await?;
    let owner = q.owner.filter(|o| !o.trim().is_empty()).or_else(|| me_name.clone()).unwrap_or_default();

    let rows = if owner.is_empty() || owner_attr.is_none() { Vec::new() } else { rows!(&db, "
        WITH latest AS (
            SELECT cu.id, cu.company_id, b.b0, b.b30, b.b60, b.b90, b.b182, b.b365, b.b366, b.total
            FROM customers cu
            LEFT JOIN LATERAL (SELECT b.* FROM balances b JOIN imports i ON i.id=b.import_id
                               WHERE b.customer_id=cu.id ORDER BY i.as_of DESC LIMIT 1) b ON true
            WHERE cu.status='active' AND ($1::bigint[] IS NULL OR cu.company_id = ANY($1))
        ),
        ranked AS (  -- Top 25 within each company, same rule as the grid highlight
            SELECT id,
              array_remove(ARRAY[
                CASE WHEN COALESCE(b366,0) > 0 AND rank() OVER (PARTITION BY company_id ORDER BY COALESCE(b366,0) DESC) <= 25 THEN '366-' END,
                CASE WHEN COALESCE(b365,0) > 0 AND rank() OVER (PARTITION BY company_id ORDER BY COALESCE(b365,0) DESC) <= 25 THEN '183-365' END,
                CASE WHEN COALESCE(b182,0) > 0 AND rank() OVER (PARTITION BY company_id ORDER BY COALESCE(b182,0) DESC) <= 25 THEN '91-182' END,
                CASE WHEN COALESCE(total,0) > 0 AND rank() OVER (PARTITION BY company_id ORDER BY COALESCE(total,0) DESC) <= 25 THEN 'Total' END
              ], NULL) AS top25
            FROM latest
        )
        SELECT cu.id, cu.ext_id, cu.name, cu.company_id, co.code AS company_code, co.name AS company_name, cu.follow_up,
               l.total, COALESCE(l.b182,0) + COALESCE(l.b365,0) + COALESCE(l.b366,0) AS over90, l.b366, l.b365, l.b182,
               r.top25, st.value AS status,
               act.last_activity, act.last_contact, act.contacts_week,
               lastone.kind AS last_kind, lastone.body AS last_body, lastone.author AS last_author,
               (SELECT string_agg(DISTINCT d.category, ', ') FROM disputes d WHERE d.customer_id=cu.id AND d.status='open') AS disputes,
               -- open disputes with no Resolution Owner: assigning one is the top priority
               (SELECT json_agg(json_build_object('id', d.id, 'category', d.category, 'amount', d.amount, 'opened_on', d.opened_on) ORDER BY d.opened_on)
                  FROM disputes d WHERE d.customer_id=cu.id AND d.status='open' AND d.resolution_owner='') AS unassigned_disputes,
               -- who to reach: the AP Contact if there is one, otherwise the first contact
               (SELECT json_build_object('title', k.title, 'name', k.name, 'phone', k.phone, 'email', k.email, 'method', k.preferred_method)
                  FROM customer_contacts k WHERE k.customer_id=cu.id ORDER BY k.is_ap DESC, k.id LIMIT 1) AS contact,
               (SELECT json_build_object('id', p.id, 'amount', p.amount, 'expected_date', p.expected_date, 'method', p.payment_method)
                  FROM promises p WHERE p.customer_id=cu.id AND p.status='open') AS promise,
               (SELECT json_build_object('id', pl.id, 'total', pl.total_amount, 'each', pl.installment_amount, 'frequency', pl.frequency, 'method', pl.payment_method,
                       'next_due', plan_next_due(pl.first_due_date, pl.frequency, pl.total_amount, pl.installment_amount, $7),
                       'last_due', plan_due(pl.first_due_date, pl.frequency, plan_payments(pl.total_amount, pl.installment_amount) - 1))
                  FROM payment_plans pl WHERE pl.customer_id=cu.id AND pl.status='open') AS plan
        FROM customers cu
        JOIN companies co ON co.id=cu.company_id
        JOIN attr_values ow ON ow.customer_id=cu.id AND ow.attribute_id=$2
        JOIN latest l ON l.id=cu.id
        JOIN ranked r ON r.id=cu.id
        LEFT JOIN attr_values st ON st.customer_id=cu.id AND st.attribute_id=$5
        LEFT JOIN LATERAL (
            SELECT MAX(a.entry_date) AS last_activity,
                   MAX(a.entry_date) FILTER (WHERE a.kind = ANY($6)) AS last_contact,
                   COUNT(*) FILTER (WHERE a.kind = ANY($6) AND a.entry_date >= $4) AS contacts_week
            FROM activities a WHERE a.customer_id=cu.id AND a.deleted_at IS NULL) act ON true
        LEFT JOIN LATERAL (
            SELECT a.kind, a.body, a.author FROM activities a WHERE a.customer_id=cu.id AND a.deleted_at IS NULL
            ORDER BY a.entry_date DESC, a.created_at DESC, a.id DESC LIMIT 1) lastone ON true
        WHERE lower(trim(ow.value)) = lower(trim($3))
        ORDER BY over90 DESC, l.total DESC NULLS LAST",
        &allowed, owner_attr, &owner, week_start, status_attr, db::CONTACT_KINDS.iter().map(|s| s.to_string()).collect::<Vec<_>>(), today) };

    // Contact attempts on this owner's accounts over the last 30 days, per company, kind and day
    // (the page filters by company and period and builds the stats and the contact-method chart).
    let contact_log = if owner.is_empty() || owner_attr.is_none() { Vec::new() } else { rows!(&db, "
        SELECT cu.company_id, a.kind, a.entry_date, COUNT(*) AS n
        FROM activities a
        JOIN customers cu ON cu.id=a.customer_id
        JOIN attr_values ow ON ow.customer_id=cu.id AND ow.attribute_id=$2
        WHERE a.deleted_at IS NULL AND a.kind = ANY($4) AND a.entry_date > $5 - 30 AND a.entry_date <= $5
          AND cu.status='active' AND ($1::bigint[] IS NULL OR cu.company_id = ANY($1))
          AND lower(trim(ow.value)) = lower(trim($3))
        GROUP BY 1, 2, 3 ORDER BY 3",
        &allowed, owner_attr, &owner, db::CONTACT_KINDS.iter().map(|s| s.to_string()).collect::<Vec<_>>(), today) };

    let status_options: Option<String> = sqlx::query_scalar("SELECT options FROM attributes WHERE name='Status'").fetch_optional(&db).await?;
    Ok(Json(json!({
        "me_name": me_name, "owner": owner, "owners": owners, "rows": rows, "contact_log": contact_log,
        "contact_kinds": db::CONTACT_KINDS,
        "today": today.to_string(), "week_start": week_start.to_string(), "contact_target": TOP25_WEEKLY_CONTACTS,
        "status_options": status_options.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!([])),
    })))
}

// ---------------------------------------------------------------- dashboard

#[derive(Deserialize)]
struct DashQ { company: Option<i64>, weeks: Option<usize> }

/// Week-over-week dashboard for one company or all companies the viewer can see.
async fn dashboard_api(State(db): State<Db>, Authenticated(p): Authenticated, Query(q): Query<DashQ>) -> R {
    staff(&p)?;
    if let Some(c) = q.company { can_company(&db, &p, c).await?; }
    let allowed: Option<Vec<i64>> = allowed_companies(&db, &p).await?.map(|s| s.into_iter().collect());
    let weeks = q.weeks.unwrap_or(12).clamp(4, 52);
    Ok(Json(dashboard::build(&db, allowed, q.company, weeks, Local::now().date_naive()).await?))
}

// ---------------------------------------------------------------- disputes

const DISPUTE_CATEGORIES: [&str; 4] = ["Work not completed", "Property or other damage", "Change in scope", "Pricing dispute"];
const DISPUTE_OUTCOMES: [&str; 6] = ["Credit or adjustment issued", "Customer paid", "Work completed / issue fixed",
    "Written off", "Withdrawn by customer", "Other"];

#[derive(Deserialize)]
struct DisputeBody {
    category: Option<String>,
    amount: Option<f64>,
    reference: Option<String>,
    description: Option<String>,
    outcome: Option<String>,
    resolution: Option<String>,
    /// Name of the employee following up on the dispute.
    resolution_owner: Option<String>,
}

fn money_s(a: Option<f64>) -> String { a.map(|a| format!(" · ${a:.2}")).unwrap_or_default() }

/// The customer a dispute belongs to, after checking the viewer can see it.
async fn can_dispute(db: &Db, p: &Principal, id: i64) -> Result<i64, AppError> {
    let cid: i64 = sqlx::query_scalar("SELECT customer_id FROM disputes WHERE id=$1").bind(id).fetch_optional(db).await?.ok_or_else(not_found)?;
    can_customer(db, p, cid).await?;
    Ok(cid)
}

async fn open_dispute(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<DisputeBody>) -> R {
    can_customer(&db, &p, id).await?;
    let category = b.category.unwrap_or_default();
    if !DISPUTE_CATEGORIES.contains(&category.as_str()) { return Err(anyhow::anyhow!("choose a dispute category").into()); }
    let description = b.description.unwrap_or_default().trim().to_string();
    if description.is_empty() { return Err(anyhow::anyhow!("describe what the customer is disputing").into()); }
    if b.amount.is_some_and(|a| a < 0.0) { return Err(anyhow::anyhow!("disputed amount can't be negative").into()); }
    let reference = b.reference.unwrap_or_default().trim().to_string();
    let mut tx = db::begin(&db, &p.user).await?;
    // Dates use the app's local day (the database clock is UTC).
    let today = Local::now().date_naive();
    let owner = b.resolution_owner.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
    // The account's Status becomes "Disputed"; remember what it was so resolving can put it back.
    let before = status_now(&mut tx, id).await?;
    let did: i64 = sqlx::query_scalar("INSERT INTO disputes (customer_id, category, amount, reference, description, opened_by, opened_on, resolution_owner, status_before)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(id).bind(&category).bind(b.amount).bind(&reference).bind(&description).bind(&p.user).bind(today).bind(&owner)
        .bind(before.as_ref().filter(|s| s.as_str() != DISPUTED_STATUS))
        .fetch_one(&mut *tx).await?;
    set_status(&mut tx, id, DISPUTED_STATUS, &p.user).await?;
    let note = format!("Dispute opened – {category}{}{}{}: {description}", money_s(b.amount),
        if reference.is_empty() { String::new() } else { format!(" · {reference}") },
        if owner.is_empty() { String::new() } else { format!(" · resolution owner {owner}") });
    exec!(&mut *tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1, $4, 'Dispute', $2, $3)", id, &note, &p.user, today);
    tx.commit().await?;
    Ok(Json(json!({"ok": true, "id": did})))
}

async fn edit_dispute(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<DisputeBody>) -> R {
    can_dispute(&db, &p, id).await?;
    if let Some(c) = &b.category { if !DISPUTE_CATEGORIES.contains(&c.as_str()) { return Err(anyhow::anyhow!("unknown dispute category").into()); } }
    if b.description.as_deref().is_some_and(|d| d.trim().is_empty()) { return Err(anyhow::anyhow!("description can't be empty").into()); }
    let mut tx = db::begin(&db, &p.user).await?;
    let n = exec!(&mut *tx, "UPDATE disputes SET category=COALESCE($2,category), amount=COALESCE($3,amount),
        reference=COALESCE($4,reference), description=COALESCE($5,description), resolution_owner=COALESCE($6,resolution_owner),
        updated_at=now() WHERE id=$1 AND status='open'",
        id, b.category.as_deref(), b.amount, b.reference.as_deref().map(str::trim), b.description.as_deref().map(str::trim),
        b.resolution_owner.as_deref().map(|o| o.split_whitespace().collect::<Vec<_>>().join(" ")));
    tx.commit().await?;
    if n == 0 { return Err(anyhow::anyhow!("only open disputes can be edited").into()); }
    Ok(Json(json!({"ok": true})))
}

async fn resolve_dispute(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<DisputeBody>) -> R {
    let cid = can_dispute(&db, &p, id).await?;
    let outcome = b.outcome.unwrap_or_default();
    if !DISPUTE_OUTCOMES.contains(&outcome.as_str()) { return Err(anyhow::anyhow!("choose how the dispute was resolved").into()); }
    let resolution = b.resolution.unwrap_or_default().trim().to_string();
    let mut tx = db::begin(&db, &p.user).await?;
    let today = Local::now().date_naive();
    let cat: Option<String> = sqlx::query_scalar("UPDATE disputes SET status='resolved', outcome=$2, resolution=$3, resolved_on=$5,
        resolved_by=$4, updated_at=now() WHERE id=$1 AND status='open' RETURNING category")
        .bind(id).bind(&outcome).bind(&resolution).bind(&p.user).bind(today).fetch_optional(&mut *tx).await?;
    let Some(cat) = cat else { return Err(anyhow::anyhow!("this dispute is already resolved").into()) };
    // Last open dispute resolved: put the Status back to what it was before the dispute (if it's still "Disputed").
    let still_open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM disputes WHERE customer_id=$1 AND status='open'").bind(cid).fetch_one(&mut *tx).await?;
    if still_open == 0 && status_now(&mut tx, cid).await?.as_deref() == Some(DISPUTED_STATUS) {
        let restore: Option<String> = sqlx::query_scalar("SELECT status_before FROM disputes WHERE customer_id=$1 AND status_before IS NOT NULL
            ORDER BY opened_on DESC, id DESC LIMIT 1").bind(cid).fetch_optional(&mut *tx).await?;
        set_status(&mut tx, cid, restore.as_deref().unwrap_or(""), &p.user).await?;
    }
    let note = format!("Dispute resolved ({cat}) – {outcome}{}", if resolution.is_empty() { String::new() } else { format!(": {resolution}") });
    exec!(&mut *tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1, $4, 'Dispute', $2, $3)", cid, &note, &p.user, today);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

async fn reopen_dispute(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    admin(&p)?;
    let cid = can_dispute(&db, &p, id).await?;
    let mut tx = db::begin(&db, &p.user).await?;
    let cat: Option<String> = sqlx::query_scalar("UPDATE disputes SET status='open', outcome=NULL, resolution=NULL, resolved_on=NULL, resolved_by=NULL,
        updated_at=now() WHERE id=$1 AND status='resolved' RETURNING category").bind(id).fetch_optional(&mut *tx).await?;
    let Some(cat) = cat else { return Err(anyhow::anyhow!("this dispute is already open").into()) };
    let before = status_now(&mut tx, cid).await?;
    if before.as_deref() != Some(DISPUTED_STATUS) {
        exec!(&mut *tx, "UPDATE disputes SET status_before=$2 WHERE id=$1", id, before.unwrap_or_default());
    }
    set_status(&mut tx, cid, DISPUTED_STATUS, &p.user).await?;
    exec!(&mut *tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1, $4, 'Dispute', $2, $3)",
        cid, format!("Dispute reopened ({cat})"), &p.user, Local::now().date_naive());
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------- contacts

const CONTACT_METHODS: [&str; 5] = ["Phone", "Email", "Text", "Mail", "Customer portal"];
const AP_CONTACT_TITLE: &str = "AP Contact";

#[derive(Deserialize)]
struct ContactBody {
    /// true = the customer's AP Contact (one per customer, titled "AP Contact")
    is_ap: Option<bool>,
    title: Option<String>,
    name: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    preferred_method: Option<String>,
}

fn tidy(s: &Option<String>) -> String { s.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ") }

fn check_contact(name: &str, phone: &str, email: &str, method: &str) -> Result<(), AppError> {
    if name.is_empty() && phone.is_empty() && email.is_empty() {
        return Err(anyhow::anyhow!("enter at least a name, phone or email").into());
    }
    if !email.is_empty() && (!email.contains('@') || email.contains(' ')) {
        return Err(anyhow::anyhow!("'{email}' doesn't look like an email address").into());
    }
    if !method.is_empty() && !CONTACT_METHODS.contains(&method) {
        return Err(anyhow::anyhow!("unknown contact method '{method}'").into());
    }
    if [name, phone, email].iter().any(|s| s.chars().count() > 200) { return Err(anyhow::anyhow!("a field is too long").into()); }
    Ok(())
}

async fn add_contact(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<ContactBody>) -> R {
    can_customer(&db, &p, id).await?;
    let is_ap = b.is_ap.unwrap_or(false);
    let title = if is_ap { AP_CONTACT_TITLE.to_string() } else { tidy(&b.title) };
    let (name, phone, email, method) = (tidy(&b.name), tidy(&b.phone), tidy(&b.email), tidy(&b.preferred_method));
    check_contact(&name, &phone, &email, &method)?;
    let mut tx = db::begin(&db, &p.user).await?;
    if is_ap {
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM customer_contacts WHERE customer_id=$1 AND is_ap").bind(id).fetch_optional(&mut *tx).await?;
        if exists.is_some() { return Err(anyhow::anyhow!("this customer already has an AP Contact – edit it instead").into()); }
    }
    let cid: i64 = sqlx::query_scalar("INSERT INTO customer_contacts (customer_id, is_ap, title, name, phone, email, preferred_method, updated_by)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(id).bind(is_ap).bind(if title.is_empty() { "Contact" } else { &title }).bind(&name).bind(&phone).bind(&email).bind(&method).bind(&p.user)
        .fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok": true, "id": cid})))
}

async fn contact_customer(db: &Db, p: &Principal, id: i64) -> Result<(i64, bool), AppError> {
    let (cust, is_ap): (i64, bool) = sqlx::query_as("SELECT customer_id, is_ap FROM customer_contacts WHERE id=$1").bind(id)
        .fetch_optional(db).await?.ok_or_else(not_found)?;
    can_customer(db, p, cust).await?;
    Ok((cust, is_ap))
}

async fn update_contact(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<ContactBody>) -> R {
    let (_, is_ap) = contact_customer(&db, &p, id).await?;
    let (name, phone, email, method) = (tidy(&b.name), tidy(&b.phone), tidy(&b.email), tidy(&b.preferred_method));
    check_contact(&name, &phone, &email, &method)?;
    let title = if is_ap { AP_CONTACT_TITLE.to_string() } else { let t = tidy(&b.title); if t.is_empty() { "Contact".into() } else { t } };
    let mut tx = db::begin(&db, &p.user).await?;
    exec!(&mut *tx, "UPDATE customer_contacts SET title=$2, name=$3, phone=$4, email=$5, preferred_method=$6, updated_at=now(), updated_by=$7 WHERE id=$1",
        id, &title, &name, &phone, &email, &method, &p.user);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

/// Removing a contact is recorded in the change history (with all its details).
async fn delete_contact(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>) -> R {
    contact_customer(&db, &p, id).await?;
    let mut tx = db::begin(&db, &p.user).await?;
    exec!(&mut *tx, "DELETE FROM customer_contacts WHERE id=$1", id);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------- Status <-> disputes

const DISPUTED_STATUS: &str = "Disputed";

/// The account's current Status value (None if not set or there is no Status attribute).
async fn status_now(tx: &mut db::Tx, cid: i64) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT v.value FROM attr_values v JOIN attributes a ON a.id=v.attribute_id WHERE v.customer_id=$1 AND a.name='Status'")
        .bind(cid).fetch_optional(&mut **tx).await?)
}

/// Set (or clear, with "") the account's Status; the change is recorded by the attr_values history trigger.
async fn set_status(tx: &mut db::Tx, cid: i64, value: &str, user: &str) -> anyhow::Result<()> {
    let Some(aid) = sqlx::query_scalar::<_, i64>("SELECT id FROM attributes WHERE name='Status'").fetch_optional(&mut **tx).await? else { return Ok(()) };
    if value.is_empty() {
        exec!(&mut **tx, "DELETE FROM attr_values WHERE customer_id=$1 AND attribute_id=$2", cid, aid);
    } else {
        exec!(&mut **tx, "INSERT INTO attr_values (customer_id, attribute_id, value, updated_by) VALUES ($1,$2,$3,$4)
            ON CONFLICT (customer_id, attribute_id) DO UPDATE SET value=excluded.value, updated_by=excluded.updated_by, updated_at=now()",
            cid, aid, value, user);
    }
    Ok(())
}

// ---------------------------------------------------------------- promises to pay

const PROMISE_STATUS: &str = "Promise to Pay";
const PAYMENT_METHODS: [&str; 6] = ["Check", "ACH / bank transfer", "Credit card", "Cash", "Customer portal", "Other"];

#[derive(Deserialize)]
struct PromiseBody {
    amount: Option<f64>,
    payment_method: Option<String>,
    expected_date: Option<String>,
    notes: Option<String>,
    /// close: kept | broken | cancelled
    outcome: Option<String>,
    note: Option<String>,
}

fn check_promise(amount: Option<f64>, method: &str, date: &Option<String>) -> Result<(f64, NaiveDate), AppError> {
    let amount = amount.filter(|a| *a > 0.0).ok_or_else(|| anyhow::anyhow!("enter the amount promised"))?;
    if !PAYMENT_METHODS.contains(&method) { return Err(anyhow::anyhow!("choose the form of payment").into()); }
    let d = date.as_deref().filter(|d| !d.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("enter the expected payment date"))?;
    Ok((amount, valid_date(d)?))
}

async fn open_promise(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<PromiseBody>) -> R {
    can_customer(&db, &p, id).await?;
    let method = tidy(&b.payment_method);
    let (amount, date) = check_promise(b.amount, &method, &b.expected_date)?;
    let notes = b.notes.unwrap_or_default().trim().to_string();
    let today = Local::now().date_naive();
    let mut tx = db::begin(&db, &p.user).await?;
    match open_commitment(&mut *tx, id).await? {
        Some(Commitment::Promise) => return Err(anyhow::anyhow!("this account already has an open promise – edit or close it first").into()),
        Some(Commitment::Plan) => return Err(anyhow::anyhow!("this account has an open payment plan – close the plan before recording a promise").into()),
        None => {}
    }
    let before = status_now(&mut tx, id).await?;
    let pid: i64 = sqlx::query_scalar("INSERT INTO promises (customer_id, amount, payment_method, expected_date, notes, status_before, created_on, created_by)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(id).bind(amount).bind(&method).bind(date).bind(&notes)
        .bind(before.as_ref().filter(|s| s.as_str() != PROMISE_STATUS)).bind(today).bind(&p.user).fetch_one(&mut *tx).await?;
    set_status(&mut tx, id, PROMISE_STATUS, &p.user).await?;
    // Follow up on the promised date (unless an earlier follow-up is already set).
    exec!(&mut *tx, "UPDATE customers SET follow_up=$2 WHERE id=$1 AND (follow_up IS NULL OR follow_up > $2 OR follow_up < $3)", id, date, today);
    let note = format!("Customer promised ${amount:.2} by {} via {method}{}", date.format("%m/%d/%Y"),
        if notes.is_empty() { String::new() } else { format!(" – {notes}") });
    exec!(&mut *tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1,$2,'Promise to pay',$3,$4)", id, today, &note, &p.user);
    tx.commit().await?;
    Ok(Json(json!({"ok": true, "id": pid})))
}

async fn promise_customer(db: &Db, p: &Principal, id: i64) -> Result<i64, AppError> {
    let cid: i64 = sqlx::query_scalar("SELECT customer_id FROM promises WHERE id=$1").bind(id).fetch_optional(db).await?.ok_or_else(not_found)?;
    can_customer(db, p, cid).await?;
    Ok(cid)
}

async fn edit_promise(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<PromiseBody>) -> R {
    let cid = promise_customer(&db, &p, id).await?;
    let method = tidy(&b.payment_method);
    let (amount, date) = check_promise(b.amount, &method, &b.expected_date)?;
    let mut tx = db::begin(&db, &p.user).await?;
    let n = exec!(&mut *tx, "UPDATE promises SET amount=$2, payment_method=$3, expected_date=$4, notes=COALESCE($5,notes), updated_at=now()
        WHERE id=$1 AND status='open'", id, amount, &method, date, b.notes.as_deref().map(str::trim));
    if n == 0 { return Err(anyhow::anyhow!("only an open promise can be edited").into()); }
    exec!(&mut *tx, "UPDATE customers SET follow_up=$2 WHERE id=$1 AND (follow_up IS NULL OR follow_up > $2)", cid, date);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

/// Close a promise as kept, broken or cancelled; the account's Status goes back to what it was before.
async fn close_promise(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<PromiseBody>) -> R {
    let cid = promise_customer(&db, &p, id).await?;
    let outcome = b.outcome.unwrap_or_default();
    if !["kept", "broken", "cancelled"].contains(&outcome.as_str()) { return Err(anyhow::anyhow!("choose kept, broken or cancelled").into()); }
    let note = b.note.unwrap_or_default().trim().to_string();
    let today = Local::now().date_naive();
    let mut tx = db::begin(&db, &p.user).await?;
    let row: Option<(f64, NaiveDate, Option<String>)> = sqlx::query_as("UPDATE promises SET status=$2, close_note=$3, closed_on=$4, closed_by=$5, updated_at=now()
        WHERE id=$1 AND status='open' RETURNING amount::float8, expected_date, status_before")
        .bind(id).bind(&outcome).bind(&note).bind(today).bind(&p.user).fetch_optional(&mut *tx).await?;
    let Some((amount, date, before)) = row else { return Err(anyhow::anyhow!("this promise is already closed").into()) };
    if status_now(&mut tx, cid).await?.as_deref() == Some(PROMISE_STATUS) {
        set_status(&mut tx, cid, before.as_deref().unwrap_or(""), &p.user).await?;
    }
    let what = match outcome.as_str() { "kept" => "kept", "broken" => "BROKEN", _ => "cancelled" };
    let body = format!("Promise to pay ${amount:.2} by {} {what}{}", date.format("%m/%d/%Y"), if note.is_empty() { String::new() } else { format!(" – {note}") });
    exec!(&mut *tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1,$2,'Promise to pay',$3,$4)", cid, today, &body, &p.user);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

/// An account has at most one open promise to pay or payment plan (each one owns the Status while open).
enum Commitment { Promise, Plan }

async fn open_commitment<'e, E: sqlx::PgExecutor<'e>>(ex: E, cid: i64) -> anyhow::Result<Option<Commitment>> {
    let kind: Option<String> = sqlx::query_scalar(
        "SELECT 'promise' FROM promises WHERE customer_id=$1 AND status='open'
         UNION ALL SELECT 'plan' FROM payment_plans WHERE customer_id=$1 AND status='open' LIMIT 1")
        .bind(cid).fetch_optional(ex).await?;
    Ok(kind.map(|k| if k == "plan" { Commitment::Plan } else { Commitment::Promise }))
}

// ---------------------------------------------------------------- payment plans

const PLAN_STATUS: &str = "Payment Plan";
const PLAN_FREQUENCIES: [&str; 3] = ["Weekly", "Bi-weekly", "Monthly"];

#[derive(Deserialize)]
struct PlanBody {
    total_amount: Option<f64>,
    installment_amount: Option<f64>,
    frequency: Option<String>,
    payment_method: Option<String>,
    first_due_date: Option<String>,
    notes: Option<String>,
    /// close: completed | defaulted | cancelled
    outcome: Option<String>,
    note: Option<String>,
}

struct PlanTerms { total: f64, each: f64, freq: String, method: String, first: NaiveDate }

fn check_plan(b: &PlanBody) -> Result<PlanTerms, AppError> {
    let total = b.total_amount.filter(|a| *a > 0.0).ok_or_else(|| anyhow::anyhow!("enter the amount agreed to"))?;
    let each = b.installment_amount.filter(|a| *a > 0.0).ok_or_else(|| anyhow::anyhow!("enter how much each payment will be"))?;
    if each > total { return Err(anyhow::anyhow!("each payment can't be more than the amount agreed to").into()); }
    let freq = tidy(&b.frequency);
    if !PLAN_FREQUENCIES.contains(&freq.as_str()) { return Err(anyhow::anyhow!("choose how often payments are made (weekly, bi-weekly or monthly)").into()); }
    let method = tidy(&b.payment_method);
    if !PAYMENT_METHODS.contains(&method.as_str()) { return Err(anyhow::anyhow!("choose the form of payment").into()); }
    let d = b.first_due_date.as_deref().filter(|d| !d.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("enter the first payment date"))?;
    Ok(PlanTerms { total, each, freq, method, first: valid_date(d)? })
}

async fn open_plan(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<PlanBody>) -> R {
    can_customer(&db, &p, id).await?;
    let t = check_plan(&b)?;
    let notes = b.notes.unwrap_or_default().trim().to_string();
    let today = Local::now().date_naive();
    let mut tx = db::begin(&db, &p.user).await?;
    match open_commitment(&mut *tx, id).await? {
        Some(Commitment::Plan) => return Err(anyhow::anyhow!("this account already has an open payment plan – edit or close it first").into()),
        Some(Commitment::Promise) => return Err(anyhow::anyhow!("this account has an open promise to pay – close the promise before setting up a plan").into()),
        None => {}
    }
    let before = status_now(&mut tx, id).await?;
    let pid: i64 = sqlx::query_scalar("INSERT INTO payment_plans (customer_id, total_amount, installment_amount, frequency, payment_method, first_due_date, notes, status_before, created_on, created_by)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id")
        .bind(id).bind(t.total).bind(t.each).bind(&t.freq).bind(&t.method).bind(t.first).bind(&notes)
        .bind(before.as_ref().filter(|s| s.as_str() != PLAN_STATUS)).bind(today).bind(&p.user).fetch_one(&mut *tx).await?;
    set_status(&mut tx, id, PLAN_STATUS, &p.user).await?;
    // Follow up on the first payment date (unless an earlier follow-up is already set).
    exec!(&mut *tx, "UPDATE customers SET follow_up=$2 WHERE id=$1 AND (follow_up IS NULL OR follow_up > $2 OR follow_up < $3)", id, t.first, today);
    let n = (t.total / t.each - 1e-9).ceil() as i64;
    let body = format!("Payment plan agreed: ${:.2} in {n} {} payment{} of ${:.2} via {}, starting {}{}", t.total, t.freq.to_lowercase(),
        if n == 1 { "" } else { "s" }, t.each, t.method, t.first.format("%m/%d/%Y"),
        if notes.is_empty() { String::new() } else { format!(" – {notes}") });
    exec!(&mut *tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1,$2,'Payment plan',$3,$4)", id, today, &body, &p.user);
    tx.commit().await?;
    Ok(Json(json!({"ok": true, "id": pid})))
}

async fn plan_customer(db: &Db, p: &Principal, id: i64) -> Result<i64, AppError> {
    let cid: i64 = sqlx::query_scalar("SELECT customer_id FROM payment_plans WHERE id=$1").bind(id).fetch_optional(db).await?.ok_or_else(not_found)?;
    can_customer(db, p, cid).await?;
    Ok(cid)
}

async fn edit_plan(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<PlanBody>) -> R {
    plan_customer(&db, &p, id).await?;
    let t = check_plan(&b)?;
    let mut tx = db::begin(&db, &p.user).await?;
    let n = exec!(&mut *tx, "UPDATE payment_plans SET total_amount=$2, installment_amount=$3, frequency=$4, payment_method=$5, first_due_date=$6,
        notes=COALESCE($7,notes), updated_at=now() WHERE id=$1 AND status='open'",
        id, t.total, t.each, &t.freq, &t.method, t.first, b.notes.as_deref().map(str::trim));
    if n == 0 { return Err(anyhow::anyhow!("only an open payment plan can be edited").into()); }
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

/// Close a plan as completed, defaulted or cancelled; the account's Status goes back to what it was before.
async fn close_plan(State(db): State<Db>, Authenticated(p): Authenticated, Path(id): Path<i64>, Json(b): Json<PlanBody>) -> R {
    let cid = plan_customer(&db, &p, id).await?;
    let outcome = b.outcome.unwrap_or_default();
    if !["completed", "defaulted", "cancelled"].contains(&outcome.as_str()) { return Err(anyhow::anyhow!("choose completed, defaulted or cancelled").into()); }
    let note = b.note.unwrap_or_default().trim().to_string();
    let today = Local::now().date_naive();
    let mut tx = db::begin(&db, &p.user).await?;
    let row: Option<(f64, f64, String, Option<String>)> = sqlx::query_as("UPDATE payment_plans SET status=$2, close_note=$3, closed_on=$4, closed_by=$5, updated_at=now()
        WHERE id=$1 AND status='open' RETURNING total_amount::float8, installment_amount::float8, frequency, status_before")
        .bind(id).bind(&outcome).bind(&note).bind(today).bind(&p.user).fetch_optional(&mut *tx).await?;
    let Some((total, each, freq, before)) = row else { return Err(anyhow::anyhow!("this payment plan is already closed").into()) };
    if status_now(&mut tx, cid).await?.as_deref() == Some(PLAN_STATUS) {
        set_status(&mut tx, cid, before.as_deref().unwrap_or(""), &p.user).await?;
    }
    let what = match outcome.as_str() { "completed" => "completed", "defaulted" => "DEFAULTED", _ => "cancelled" };
    let body = format!("Payment plan ${total:.2} (${each:.2} {}) {what}{}", freq.to_lowercase(), if note.is_empty() { String::new() } else { format!(" – {note}") });
    exec!(&mut *tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1,$2,'Payment plan',$3,$4)", cid, today, &body, &p.user);
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}
