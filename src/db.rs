//! PostgreSQL connection, migrations and small query helpers.
//!
//! Tables live in their own schema (default `ar`), created if missing and put first on
//! every connection's search_path. Migrations in `migrations/` run on start-up.

use anyhow::{bail, Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool, Postgres, Transaction};
use std::str::FromStr;

pub type Tx = Transaction<'static, Postgres>;

/// Activity types that count as customer contact (they update "Most Recent Contact Date/Method").
pub const CONTACT_KINDS: [&str; 5] = ["Call", "Voicemail", "Email", "Text", "Site visit"];

/// Run a SELECT and return its rows as JSON objects (Postgres builds the JSON, so column
/// types need no mapping: NUMERIC -> number, DATE -> "YYYY-MM-DD", TIMESTAMPTZ -> ISO text).
#[macro_export]
macro_rules! rows {
    ($ex:expr, $sql:expr $(, $b:expr)* $(,)?) => {{
        let sql = format!("SELECT COALESCE(json_agg(t), '[]'::json) FROM ({}) t", $sql);
        let v: serde_json::Value = sqlx::query_scalar(&sql)$(.bind($b))*.fetch_one($ex).await?;
        match v { serde_json::Value::Array(a) => a, _ => Vec::new() }
    }};
}

/// Run a statement and return the number of rows affected.
#[macro_export]
macro_rules! exec {
    ($ex:expr, $sql:expr $(, $b:expr)* $(,)?) => {{
        sqlx::query($sql)$(.bind($b))*.execute($ex).await?.rows_affected()
    }};
}

fn valid_schema(s: &str) -> Result<()> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') || s.starts_with(|c: char| c.is_ascii_digit()) {
        bail!("invalid schema name '{s}' (use lower-case letters, digits and _)");
    }
    Ok(())
}

/// Connect, create the schema if needed, run migrations and seed defaults.
pub async fn connect(url: &str, schema: &str) -> Result<PgPool> {
    valid_schema(schema)?;
    let opts = PgConnectOptions::from_str(url).context("invalid database URL")?;
    // Create the schema on a plain connection first (migrations need it on the search_path).
    {
        let boot = PgPoolOptions::new().max_connections(1).connect_with(opts.clone()).await
            .with_context(|| format!("could not connect to PostgreSQL at {}", redact(url)))?;
        boot.execute(format!("CREATE SCHEMA IF NOT EXISTS {schema}").as_str()).await?;
        boot.close().await;
    }
    let search_path = format!("SET search_path TO {schema}");
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .after_connect(move |conn, _| {
            let sp = search_path.clone();
            Box::pin(async move { conn.execute(sp.as_str()).await.map(|_| ()) })
        })
        .connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&pool).await.context("database migration failed")?;
    seed(&pool).await?;
    Ok(pool)
}

fn redact(url: &str) -> String {
    // postgres://user:password@host -> postgres://user:***@host
    match (url.find("://"), url.find('@')) {
        (Some(s), Some(at)) if url[s + 3..at].contains(':') => {
            let colon = s + 3 + url[s + 3..at].find(':').unwrap();
            format!("{}:***{}", &url[..colon], &url[at..])
        }
        _ => url.to_string(),
    }
}

/// Begin a transaction that records `user` as the actor in the history tables.
pub async fn begin(pool: &PgPool, user: &str) -> Result<Tx> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('ar.user', $1, true)").bind(user).execute(&mut *tx).await?;
    Ok(tx)
}

/// Seed the global attribute list with the columns used in the example workbooks.
async fn seed(pool: &PgPool) -> Result<()> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attributes").fetch_one(pool).await?;
    if n > 0 {
        return Ok(());
    }
    let status_opts = serde_json::json!([
        {"label": "Payment Plan", "color": "#c6efce"},
        {"label": "Requesting 3rd party collections", "color": "#ffc7ce"},
        {"label": "Requesting write off", "color": "#e4dfec"},
        {"label": "Flagged for attention", "color": "#ffeb9c"},
        {"label": "Paid - pending removal", "color": "#ddebf7"},
        {"label": "Disputed", "color": "#f9cbb7"},
        {"label": "Promise to Pay", "color": "#d9f2e3"}
    ])
    .to_string();
    let rows: &[(&str, &str, &str, i32, bool)] = &[
        ("Status", "select", &status_opts, 10, true),
        ("Account Owner", "text", "[]", 20, false),
        ("Last Known Payment Date", "date", "[]", 30, true),
        ("Last Known Payment Amount", "money", "[]", 40, true),
        ("Most Recent Contact Date", "date", "[]", 50, true),
        ("Most Recent Contact Method", "text", "[]", 60, true),
        ("Lien Letter Sent", "date", "[]", 70, false),
        ("Lien Filed", "date", "[]", 80, false),
        ("APR Referral Submitted", "date", "[]", 90, false),
    ];
    for (name, kind, opts, sort, on) in rows {
        sqlx::query("INSERT INTO attributes (name, kind, options, sort, default_on) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (name) DO NOTHING")
            .bind(name).bind(kind).bind(opts).bind(sort).bind(on).execute(pool).await?;
    }
    Ok(())
}

/// Find or create a company by its location code; new companies get the default-on attributes.
pub async fn ensure_company(tx: &mut Tx, code: &str, name: &str, legal: &str) -> Result<i64> {
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM companies WHERE code=$1").bind(code).fetch_optional(&mut **tx).await? {
        return Ok(id);
    }
    let id: i64 = sqlx::query_scalar("INSERT INTO companies (code, name, legal_name) VALUES ($1,$2,$3) RETURNING id")
        .bind(code).bind(name).bind(legal).fetch_one(&mut **tx).await?;
    sqlx::query("INSERT INTO company_attributes (company_id, attribute_id, enabled)
                 SELECT $1, id, true FROM attributes WHERE default_on AND NOT archived")
        .bind(id).execute(&mut **tx).await?;
    Ok(id)
}

/// Test helper: a fresh, uniquely named schema on the development server, dropped by `drop_test_schema`.
#[cfg(test)]
pub async fn test_pool() -> (PgPool, String) {
    let url = std::env::var("AR_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://postgres@localhost:5433/ar_tracker".into());
    let schema = format!("test_{}_{}", std::process::id(), rand_suffix());
    let pool = connect(&url, &schema).await.unwrap_or_else(|e| panic!(
        "tests need the development PostgreSQL ({url}): set AR_TEST_DATABASE_URL or start a local server – {e:#}"));
    (pool, schema)
}
#[cfg(test)]
fn rand_suffix() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() ^ N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
#[cfg(test)]
pub async fn drop_test_schema(pool: PgPool, schema: &str) {
    let _ = pool.execute(format!("DROP SCHEMA {schema} CASCADE").as_str()).await;
    pool.close().await;
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn activity_history_is_permanent() {
        let (pool, schema) = super::test_pool().await;
        let mut tx = super::begin(&pool, "sc").await.unwrap();
        let co = super::ensure_company(&mut tx, "1001", "Sample Co", "").await.unwrap();
        let cid: i64 = sqlx::query_scalar("INSERT INTO customers (company_id, ext_id, name, first_seen, last_seen) VALUES ($1,'SL_1','Smith','2026-11-11','2026-11-11') RETURNING id")
            .bind(co).fetch_one(&mut *tx).await.unwrap();
        let aid: i64 = sqlx::query_scalar("INSERT INTO activities (customer_id, entry_date, kind, body, author) VALUES ($1,'2026-11-15','Call','Left VM','sc') RETURNING id")
            .bind(cid).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = super::begin(&pool, "sc").await.unwrap();
        sqlx::query("UPDATE activities SET body='Left VM, promised to pay Friday', edited_at=now() WHERE id=$1").bind(aid).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = super::begin(&pool, "jw").await.unwrap();
        sqlx::query("UPDATE activities SET deleted_at=now(), deleted_by='jw' WHERE id=$1").bind(aid).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();

        let v: Vec<(String, String, String)> = sqlx::query_as("SELECT change, body, changed_by FROM activity_versions WHERE activity_id=$1 ORDER BY id")
            .bind(aid).fetch_all(&pool).await.unwrap();
        assert_eq!(v, vec![
            ("created".into(), "Left VM".into(), "sc".into()),
            ("edited".into(), "Left VM, promised to pay Friday".into(), "sc".into()),
            ("deleted".into(), "Left VM, promised to pay Friday".into(), "jw".into()),
        ]);
        // A field change, so the change log has a row to protect.
        let mut tx = super::begin(&pool, "sc").await.unwrap();
        sqlx::query("INSERT INTO attr_values (customer_id, attribute_id, value) SELECT $1, id, 'Payment Plan' FROM attributes WHERE name='Status'").bind(cid).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let by: String = sqlx::query_scalar("SELECT changed_by FROM change_log WHERE customer_id=$1").bind(cid).fetch_one(&pool).await.unwrap();
        assert_eq!(by, "sc");
        // The entry and its history cannot be removed or rewritten.
        assert!(sqlx::query("DELETE FROM activities WHERE id=$1").bind(aid).execute(&pool).await.is_err());
        assert!(sqlx::query("UPDATE activity_versions SET body='x'").execute(&pool).await.is_err());
        assert!(sqlx::query("DELETE FROM change_log").execute(&pool).await.is_err());
        assert!(sqlx::query("TRUNCATE change_log").execute(&pool).await.is_err());
        assert!(sqlx::query("TRUNCATE activities CASCADE").execute(&pool).await.is_err());
        super::drop_test_schema(pool, &schema).await;
    }

    #[tokio::test]
    async fn company_settings_changes_are_logged() {
        let (pool, schema) = super::test_pool().await;
        let mut tx = super::begin(&pool, "sc").await.unwrap();
        let co = super::ensure_company(&mut tx, "1001", "Sample Co", "").await.unwrap();
        for v in [r#"["Cheryl Smith"]"#, r#"["Cheryl Smith","Monica Aceves"]"#] {
            sqlx::query("INSERT INTO company_settings (company_id, key, value) VALUES ($1,'account_owners',$2::jsonb)
                ON CONFLICT (company_id, key) DO UPDATE SET value=excluded.value").bind(co).bind(v).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        let log: Vec<(Option<String>, String, String)> = sqlx::query_as("SELECT old_value::text, new_value::text, changed_by FROM company_settings_log ORDER BY id")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(log, vec![
            (None, r#"["Cheryl Smith"]"#.into(), "sc".into()),
            (Some(r#"["Cheryl Smith"]"#.into()), r#"["Cheryl Smith", "Monica Aceves"]"#.into(), "sc".into()),
        ]);
        assert!(sqlx::query("DELETE FROM company_settings_log").execute(&pool).await.is_err());
        super::drop_test_schema(pool, &schema).await;
    }

    #[tokio::test]
    async fn disputes_are_logged_and_never_deleted() {
        let (pool, schema) = super::test_pool().await;
        let mut tx = super::begin(&pool, "sc").await.unwrap();
        let co = super::ensure_company(&mut tx, "1001", "Sample Co", "").await.unwrap();
        let cid: i64 = sqlx::query_scalar("INSERT INTO customers (company_id, ext_id, name, first_seen, last_seen) VALUES ($1,'SL_1','Smith','2026-11-11','2026-11-11') RETURNING id")
            .bind(co).fetch_one(&mut *tx).await.unwrap();
        let did: i64 = sqlx::query_scalar("INSERT INTO disputes (customer_id, category, amount, reference, description) VALUES ($1,'Pricing dispute',1250,'INV 9','Quoted lower') RETURNING id")
            .bind(cid).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = super::begin(&pool, "jw").await.unwrap();
        sqlx::query("UPDATE disputes SET status='resolved', outcome='Credit or adjustment issued' WHERE id=$1").bind(did).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let log: Vec<(Option<String>, String, String)> = sqlx::query_as("SELECT old_value, new_value, changed_by FROM change_log WHERE field='dispute' ORDER BY id")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(log, vec![
            (None, "Pricing dispute · $1,250.00 · INV 9 · open".into(), "sc".into()),
            (Some("Pricing dispute · $1,250.00 · INV 9 · open".into()), "Pricing dispute · $1,250.00 · INV 9 · resolved · Credit or adjustment issued".into(), "jw".into()),
        ]);
        assert!(sqlx::query("DELETE FROM disputes WHERE id=$1").bind(did).execute(&pool).await.is_err());
        super::drop_test_schema(pool, &schema).await;
    }

    #[tokio::test]
    async fn one_ap_contact_and_contact_changes_logged() {
        let (pool, schema) = super::test_pool().await;
        let mut tx = super::begin(&pool, "sc").await.unwrap();
        let co = super::ensure_company(&mut tx, "1001", "Sample Co", "").await.unwrap();
        let cid: i64 = sqlx::query_scalar("INSERT INTO customers (company_id, ext_id, name, first_seen, last_seen) VALUES ($1,'SL_1','Smith','2026-11-11','2026-11-11') RETURNING id")
            .bind(co).fetch_one(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO customer_contacts (customer_id, is_ap, title, name, email, preferred_method) VALUES ($1, true, 'AP Contact', 'Ana', 'ap@x.com', 'Email')")
            .bind(cid).execute(&mut *tx).await.unwrap();
        let other: i64 = sqlx::query_scalar("INSERT INTO customer_contacts (customer_id, title, name, phone) VALUES ($1, 'Owner', 'Bob', '555-0100') RETURNING id")
            .bind(cid).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        // A second AP Contact is refused by the database.
        assert!(sqlx::query("INSERT INTO customer_contacts (customer_id, is_ap, title, name) VALUES ($1, true, 'AP Contact', 'Dup')").bind(cid).execute(&pool).await.is_err());
        let mut tx = super::begin(&pool, "jw").await.unwrap();
        sqlx::query("DELETE FROM customer_contacts WHERE id=$1").bind(other).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let log: Vec<(Option<String>, Option<String>, String)> = sqlx::query_as("SELECT old_value, new_value, changed_by FROM change_log WHERE field='contact' ORDER BY id")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(log, vec![
            (None, Some("AP Contact · Ana · ap@x.com · prefers Email".into()), "sc".into()),
            (None, Some("Owner · Bob · 555-0100".into()), "sc".into()),
            (Some("Owner · Bob · 555-0100".into()), None, "jw".into()),
        ]);
        super::drop_test_schema(pool, &schema).await;
    }

    #[tokio::test]
    async fn one_open_promise_and_promise_history() {
        let (pool, schema) = super::test_pool().await;
        let mut tx = super::begin(&pool, "sc").await.unwrap();
        let co = super::ensure_company(&mut tx, "1001", "Sample Co", "").await.unwrap();
        let cid: i64 = sqlx::query_scalar("INSERT INTO customers (company_id, ext_id, name, first_seen, last_seen) VALUES ($1,'SL_1','Smith','2026-11-11','2026-11-11') RETURNING id")
            .bind(co).fetch_one(&mut *tx).await.unwrap();
        let p1: i64 = sqlx::query_scalar("INSERT INTO promises (customer_id, amount, payment_method, expected_date, created_on) VALUES ($1, 2000, 'Check', '2026-11-20', '2026-11-15') RETURNING id")
            .bind(cid).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        // Only one open promise per account; amounts must be positive.
        assert!(sqlx::query("INSERT INTO promises (customer_id, amount, payment_method, expected_date, created_on) VALUES ($1, 5, 'Cash', '2026-11-21', '2026-11-15')")
            .bind(cid).execute(&pool).await.is_err());
        let mut tx = super::begin(&pool, "jw").await.unwrap();
        sqlx::query("UPDATE promises SET status='kept' WHERE id=$1").bind(p1).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert!(sqlx::query("INSERT INTO promises (customer_id, amount, payment_method, expected_date, created_on) VALUES ($1, 0, 'Cash', '2026-11-21', '2026-11-15')")
            .bind(cid).execute(&pool).await.is_err());
        let log: Vec<(Option<String>, String, String)> = sqlx::query_as("SELECT old_value, new_value, changed_by FROM change_log WHERE field='promise' ORDER BY id")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(log, vec![
            (None, "$2,000.00 · Check · by 11/20/2026 · open".into(), "sc".into()),
            (Some("$2,000.00 · Check · by 11/20/2026 · open".into()), "$2,000.00 · Check · by 11/20/2026 · kept".into(), "jw".into()),
        ]);
        assert!(sqlx::query("DELETE FROM promises WHERE id=$1").bind(p1).execute(&pool).await.is_err());
        super::drop_test_schema(pool, &schema).await;
    }
}
