//! Parses the accounting system "Customer aging report" export (one sheet per location)
//! and applies it to the database as a weekly snapshot.

use anyhow::{anyhow, bail, Context, Result};
use calamine::{open_workbook_auto_from_rs, Data, Reader};
use chrono::{Duration, NaiveDate};
use crate::db::Tx;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Cursor;

/// Standard display buckets used by the formatted workbooks.
pub const BUCKETS: [&str; 7] = ["-0", "1-30", "31-60", "61-90", "91-182", "183-365", "366-"];
const BUCKET_COLS: [&str; 7] = ["b0", "b30", "b60", "b90", "b182", "b365", "b366"];

fn target(age: i64) -> usize {
    match age {
        i64::MIN..=0 => 0,
        1..=30 => 1,
        31..=60 => 2,
        61..=90 => 3,
        91..=182 => 4,
        183..=365 => 5,
        _ => 6,
    }
}

/// A report bucket column such as "-0", "91-120" or "121-".
#[derive(Clone, Debug)]
struct SrcBucket {
    label: String,
    col: usize,
    lo: Option<i64>,
    hi: Option<i64>,
}

impl SrcBucket {
    fn parse(label: &str, col: usize) -> Option<Self> {
        let l = label.trim().trim_end_matches('+');
        let (a, b) = l.split_once('-')?;
        let p = |s: &str| -> Option<Option<i64>> {
            if s.trim().is_empty() { Some(None) } else { s.trim().parse().ok().map(Some) }
        };
        let (lo, hi) = (p(a)?, p(b)?);
        // "-0" means "not yet due": no lower bound, upper bound 0.
        Some(SrcBucket { label: label.trim().to_string(), col, lo, hi })
    }

    /// Map an amount in this report bucket to a standard display bucket. Report buckets that
    /// straddle display buckets (e.g. "121-") are split using the age since GL posting date.
    fn map(&self, as_of: NaiveDate, posting: Option<NaiveDate>) -> usize {
        let t_lo = self.lo.map(target).unwrap_or(0);
        let t_hi = self.hi.map(target).unwrap_or(6);
        if t_lo == t_hi {
            return t_lo;
        }
        let Some(p) = posting else { return t_lo };
        let mut age = (as_of - p).num_days();
        if let Some(lo) = self.lo { age = age.max(lo); }
        if let Some(hi) = self.hi { age = age.min(hi); }
        target(age)
    }
}

#[derive(Debug)]
pub struct Line {
    pub ext_id: String,
    pub name: String,
    pub posting: Option<NaiveDate>,
    pub src: String,
    pub bucket: usize,
    pub amount: f64,
}

#[derive(Debug)]
pub struct SheetData {
    pub sheet: String,
    pub code: String,
    pub location: String,
    pub legal_name: String,
    pub as_of: NaiveDate,
    pub lines: Vec<Line>,
    pub grand_total: Option<f64>,
}

fn excel_date(serial: f64) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(1899, 12, 30)?.checked_add_signed(Duration::days(serial.floor() as i64))
}

fn cell_date(c: &Data) -> Option<NaiveDate> {
    match c {
        Data::DateTime(d) => excel_date(d.as_f64()),
        Data::Float(f) if *f > 20000.0 && *f < 80000.0 => excel_date(*f),
        Data::String(s) | Data::DateTimeIso(s) => parse_date_str(s),
        _ => None,
    }
}

fn parse_date_str(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    for f in ["%m/%d/%Y", "%Y-%m-%d", "%m/%d/%y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, f) {
            return Some(d);
        }
    }
    s.get(..10).and_then(|p| NaiveDate::parse_from_str(p, "%Y-%m-%d").ok())
}

fn cell_str(c: &Data) -> String {
    match c {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        other => other.to_string().trim().to_string(),
    }
}

fn cell_num(c: &Data) -> f64 {
    match c {
        Data::Float(f) => *f,
        Data::Int(i) => *i as f64,
        Data::String(s) => s.replace([',', '$'], "").trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

pub fn parse_workbook(bytes: Vec<u8>) -> Result<Vec<SheetData>> {
    let mut wb = open_workbook_auto_from_rs(Cursor::new(bytes)).context("not a readable Excel file")?;
    let mut out = Vec::new();
    for name in wb.sheet_names().to_owned() {
        let range = wb.worksheet_range(&name)?;
        if let Some(sd) = parse_sheet(&name, &range)? {
            out.push(sd);
        }
    }
    Ok(out)
}

fn parse_sheet(sheet: &str, range: &calamine::Range<Data>) -> Result<Option<SheetData>> {
    let rows: Vec<&[Data]> = range.rows().collect();
    let get = |r: &[Data], i: usize| r.get(i).cloned().unwrap_or(Data::Empty);

    let mut legal = String::new();
    let mut location = None;
    let mut as_of = None;
    let mut header = None;
    for (i, r) in rows.iter().enumerate() {
        let a = cell_str(&get(r, 0));
        match a.as_str() {
            "Company name:" => legal = cell_str(&get(r, 1)),
            "Location:" => location = Some(cell_str(&get(r, 1))),
            "As of date:" => as_of = cell_date(&get(r, 1)),
            _ => {}
        }
        if cell_str(&get(r, 1)).eq_ignore_ascii_case("Customer ID") {
            header = Some(i);
            break;
        }
    }
    let (Some(location), Some(h)) = (location, header) else { return Ok(None) };
    let as_of = as_of.ok_or_else(|| anyhow!("sheet '{sheet}': missing 'As of date:'"))?;
    let (code, loc_name) = match location.split_once("--") {
        Some((c, n)) => (c.trim().to_string(), n.trim().to_string()),
        None => (location.clone(), location.clone()),
    };

    let mut buckets = Vec::new();
    let mut total_col = None;
    for (ci, c) in rows[h].iter().enumerate().skip(3) {
        let l = cell_str(c);
        if l.eq_ignore_ascii_case("Total") {
            total_col = Some(ci);
        } else if let Some(b) = SrcBucket::parse(&l, ci) {
            buckets.push(b);
        }
    }
    if buckets.is_empty() {
        bail!("sheet '{sheet}': no aging bucket columns found in header row");
    }

    let mut lines = Vec::new();
    let mut grand_total = None;
    let mut cur_date: Option<NaiveDate> = None;
    for r in &rows[h + 1..] {
        let a = get(r, 0);
        let id = cell_str(&get(r, 1));
        let a_str = cell_str(&a);
        if id.eq_ignore_ascii_case("Grand totals") {
            grand_total = total_col.map(|c| cell_num(&get(r, c)));
            continue;
        }
        if a_str.starts_with("Total for") || id.is_empty() {
            continue;
        }
        // Rows with a blank posting date belong to the date group above them.
        if let Some(d) = cell_date(&a) {
            cur_date = Some(d);
        }
        let name = cell_str(&get(r, 2));
        for b in &buckets {
            let amt = cell_num(&get(r, b.col));
            if amt.abs() < 0.0001 {
                continue;
            }
            lines.push(Line {
                ext_id: id.clone(),
                name: name.clone(),
                posting: cur_date,
                src: b.label.clone(),
                bucket: b.map(as_of, cur_date),
                amount: amt,
            });
        }
    }

    Ok(Some(SheetData { sheet: sheet.to_string(), code, location: loc_name, legal_name: legal, as_of, lines, grand_total }))
}

#[derive(Serialize, Debug)]
pub struct SheetResult {
    pub sheet: String,
    pub company: String,
    pub as_of: String,
    pub status: String, // imported | replaced | skipped | error
    pub message: String,
    pub customers: usize,
    pub added: usize,
    pub archived: usize,
    pub reactivated: usize,
    pub total: f64,
}

/// Apply one parsed sheet as a snapshot. Customers missing from the snapshot are archived
/// (their notes and attributes are kept); customers that come back are reactivated.
pub async fn apply_sheet(tx: &mut Tx, sd: &SheetData, filename: &str, user: &str) -> Result<SheetResult> {
    let mut res = SheetResult {
        sheet: sd.sheet.clone(),
        company: format!("{} – {}", sd.code, sd.location),
        as_of: sd.as_of.to_string(),
        status: "imported".into(),
        message: String::new(),
        customers: 0, added: 0, archived: 0, reactivated: 0, total: 0.0,
    };
    if sd.lines.is_empty() {
        res.status = "skipped".into();
        res.message = "No balances on this sheet (nothing changed).".into();
        return Ok(res);
    }
    let as_of = sd.as_of;
    let company_id = crate::db::ensure_company(tx, &sd.code, &sd.location, &sd.legal_name).await?;

    let latest: Option<NaiveDate> = sqlx::query_scalar("SELECT MAX(as_of) FROM imports WHERE company_id=$1")
        .bind(company_id).fetch_one(&mut **tx).await?;
    if let Some(l) = latest {
        if l > as_of {
            res.status = "error".into();
            res.message = format!("Report is dated {as_of}, older than the latest import ({l}). Not applied.");
            return Ok(res);
        }
    }
    let replaced = crate::exec!(&mut **tx, "DELETE FROM imports WHERE company_id=$1 AND as_of=$2", company_id, as_of) > 0;
    if replaced {
        res.status = "replaced".into();
        res.message = "Replaced an earlier import with the same as-of date.".into();
    }
    let import_id: i64 = sqlx::query_scalar("INSERT INTO imports (company_id, as_of, filename, imported_by) VALUES ($1,$2,$3,$4) RETURNING id")
        .bind(company_id).bind(as_of).bind(filename).bind(user).fetch_one(&mut **tx).await?;

    // Aggregate lines per customer.
    let mut per: HashMap<String, (String, [f64; 7])> = HashMap::new();
    let mut order = Vec::new();
    for l in &sd.lines {
        let e = per.entry(l.ext_id.clone()).or_insert_with(|| {
            order.push(l.ext_id.clone());
            (l.name.clone(), [0.0; 7])
        });
        e.1[l.bucket] += l.amount;
    }

    let mut ids: HashMap<String, i64> = HashMap::new();
    for ext in &order {
        let (name, b) = &per[ext];
        let existing: Option<(i64, String)> = sqlx::query_as("SELECT id, status FROM customers WHERE company_id=$1 AND ext_id=$2")
            .bind(company_id).bind(ext).fetch_optional(&mut **tx).await?;
        let cid = match existing {
            Some((id, status)) => {
                if status == "archived" {
                    res.reactivated += 1;
                }
                crate::exec!(&mut **tx, "UPDATE customers SET name=$2, status='active', last_seen=$3, archived_on=NULL WHERE id=$1", id, name, as_of);
                id
            }
            None => {
                res.added += 1;
                sqlx::query_scalar("INSERT INTO customers (company_id, ext_id, name, first_seen, last_seen) VALUES ($1,$2,$3,$4,$4) RETURNING id")
                    .bind(company_id).bind(ext).bind(name).bind(as_of).fetch_one(&mut **tx).await?
            }
        };
        let total: f64 = b.iter().sum();
        res.total += total;
        let sql = format!("INSERT INTO balances (import_id, customer_id, {}, total) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)", BUCKET_COLS.join(","));
        crate::exec!(&mut **tx, &sql, import_id, cid, b[0], b[1], b[2], b[3], b[4], b[5], b[6], total);
        ids.insert(ext.clone(), cid);
    }
    for l in &sd.lines {
        crate::exec!(&mut **tx, "INSERT INTO lines (import_id, customer_id, posting_date, source_bucket, bucket, amount) VALUES ($1,$2,$3,$4,$5,$6)",
            import_id, ids[&l.ext_id], l.posting, &l.src, BUCKETS[l.bucket], l.amount);
    }

    // Anyone active but not on this report has paid off / cleared: move to the archive.
    res.archived = crate::exec!(&mut **tx,
        "UPDATE customers SET status='archived', archived_on=$2
         WHERE company_id=$1 AND status='active'
           AND id NOT IN (SELECT customer_id FROM balances WHERE import_id=$3)",
        company_id, as_of, import_id) as usize;
    res.customers = order.len();
    res.total = (res.total * 100.0).round() / 100.0;
    crate::exec!(&mut **tx, "UPDATE imports SET line_count=$2, total=$3, added=$4, archived=$5, reactivated=$6 WHERE id=$1",
        import_id, sd.lines.len() as i32, res.total, res.added as i32, res.archived as i32, res.reactivated as i32);

    if let Some(gt) = sd.grand_total {
        if (gt - res.total).abs() > 0.01 {
            res.message = format!("{} Warning: imported total {:.2} does not match report grand total {:.2}.", res.message, res.total, gt).trim().to_string();
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to a real customer aging report, from `AR_SAMPLE_REPORT`. The test is skipped when it is not set.
    fn sample() -> Option<Vec<u8>> { std::fs::read(std::env::var("AR_SAMPLE_REPORT").ok()?).ok() }

    fn copy_line(l: &Line) -> Line {
        Line { ext_id: l.ext_id.clone(), name: l.name.clone(), posting: l.posting, src: l.src.clone(), bucket: l.bucket, amount: l.amount }
    }

    async fn apply(pool: &sqlx::PgPool, sd: &SheetData, f: &str) -> SheetResult {
        let mut tx = crate::db::begin(pool, "T").await.unwrap();
        let r = apply_sheet(&mut tx, sd, f, "T").await.unwrap();
        tx.commit().await.unwrap();
        r
    }

    #[tokio::test]
    async fn weekly_rollover_archives_and_reactivates() {
        let Some(bytes) = sample() else { eprintln!("AR_SAMPLE_REPORT not set; skipping"); return };
        let (pool, schema) = crate::db::test_pool().await;
        let mut sheets = parse_workbook(bytes).unwrap();
        let sd = sheets.iter_mut().find(|s| !s.lines.is_empty()).unwrap();

        let r1 = apply(&pool, sd, "wk1.xlsx").await;
        assert_eq!(r1.customers, 241);
        assert!(r1.message.is_empty(), "{}", r1.message);

        // Note on the first customer, then next week's report without them.
        let gone = sd.lines[0].ext_id.clone();
        let cid: i64 = sqlx::query_scalar("SELECT id FROM customers WHERE ext_id=$1").bind(&gone).fetch_one(&pool).await.unwrap();
        sqlx::query("INSERT INTO activities (customer_id, entry_date, body) VALUES ($1,'2026-11-09','promised to pay')").bind(cid).execute(&pool).await.unwrap();
        let saved: Vec<Line> = sd.lines.iter().filter(|l| l.ext_id == gone).map(copy_line).collect();
        sd.lines.retain(|l| l.ext_id != gone);
        sd.as_of += Duration::days(7);
        let r2 = apply(&pool, sd, "wk2.xlsx").await;
        assert_eq!((r2.archived, r2.added, r2.reactivated), (1, 0, 0));
        let (status, notes): (String, i64) = sqlx::query_as(
            "SELECT status, (SELECT COUNT(*) FROM activities WHERE customer_id=c.id) FROM customers c WHERE id=$1").bind(cid).fetch_one(&pool).await.unwrap();
        assert_eq!((status.as_str(), notes), ("archived", 1));
        // The archive is recorded in the change log, attributed to the importing user.
        let by: String = sqlx::query_scalar("SELECT changed_by FROM change_log WHERE customer_id=$1 AND field='status' AND new_value='archived'")
            .bind(cid).fetch_one(&pool).await.unwrap();
        assert_eq!(by, "T");

        // An older report is refused.
        sd.as_of -= Duration::days(14);
        assert_eq!(apply(&pool, sd, "old.xlsx").await.status, "error");

        // They show up again the week after: reactivated with notes intact.
        sd.lines.extend(saved);
        sd.as_of += Duration::days(21);
        let r3 = apply(&pool, sd, "wk3.xlsx").await;
        assert_eq!((r3.archived, r3.reactivated), (0, 1));
        crate::db::drop_test_schema(pool, &schema).await;
    }
}
