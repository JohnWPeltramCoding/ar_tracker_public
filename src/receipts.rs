//! Customer receipts register import: finds each customer's most recent payment
//! (same-day receipts totalled) and updates "Last Known Payment Date / Amount".

use anyhow::{anyhow, bail, Context, Result};
use calamine::{open_workbook_auto_from_rs, Data, Reader};
use chrono::{Duration, NaiveDate};
use crate::db::Tx;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;

pub const PAY_DATE: &str = "Last Known Payment Date";
pub const PAY_AMOUNT: &str = "Last Known Payment Amount";

#[derive(Debug, Clone)]
#[allow(dead_code)] // name is kept for diagnostics and tests
pub struct Receipt {
    pub ext_id: String,
    pub name: String,
    pub date: NaiveDate,
    pub amount: f64,
    pub method: String,
    pub reference: String,
}

#[derive(Serialize, Default, Debug)]
pub struct ReceiptResult {
    pub file: String,
    pub receipts: usize,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
    pub total: f64,
    pub customers_in_file: usize,
    pub matched_open: usize,
    pub matched_archived: usize,
    pub updated: usize,
    pub already_current: usize,
    pub kept_newer: usize,
    pub not_in_app: usize,
    pub ambiguous: Vec<String>,
    /// Individual receipts newly stored for the dashboard (matched customers only; repeats skipped).
    pub receipts_stored: usize,
    /// Up to 15 examples of what was set: (customer, date, amount, receipts that day)
    pub examples: Vec<(String, String, f64, usize)>,
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    ["%m/%d/%Y", "%Y-%m-%d", "%m/%d/%y"].iter().find_map(|f| NaiveDate::parse_from_str(s, f).ok())
}
fn parse_amount(s: &str) -> Option<f64> {
    let t = s.trim().replace([',', '$'], "");
    let (neg, t) = match t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) { Some(x) => (true, x.to_string()), None => (false, t) };
    t.parse::<f64>().ok().map(|v| if neg { -v } else { v })
}

/// Read the file into rows of strings (CSV, or the first sheet of an Excel workbook).
fn rows_of(bytes: Vec<u8>, filename: &str) -> Result<Vec<Vec<String>>> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".xlsx") || lower.ends_with(".xlsm") || lower.ends_with(".xls") {
        let mut wb = open_workbook_auto_from_rs(Cursor::new(bytes)).context("not a readable Excel file")?;
        let name = wb.sheet_names().first().cloned().ok_or_else(|| anyhow!("workbook has no sheets"))?;
        let range = wb.worksheet_range(&name)?;
        return Ok(range.rows().map(|r| r.iter().map(|c| match c {
            Data::Empty => String::new(),
            Data::DateTime(d) => NaiveDate::from_ymd_opt(1899, 12, 30).unwrap()
                .checked_add_signed(Duration::days(d.as_f64().floor() as i64)).map(|d| d.format("%m/%d/%Y").to_string()).unwrap_or_default(),
            Data::Float(f) => format!("{f}"),
            o => o.to_string(),
        }).collect()).collect());
    }
    let text = String::from_utf8_lossy(&bytes).trim_start_matches('\u{feff}').to_string();
    let mut rd = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(text.as_bytes());
    let mut out = Vec::new();
    for rec in rd.records() {
        out.push(rec.context("could not read the CSV")?.iter().map(|s| s.to_string()).collect());
    }
    Ok(out)
}

pub fn parse(bytes: Vec<u8>, filename: &str) -> Result<Vec<Receipt>> {
    let rows = rows_of(bytes, filename)?;
    let norm = |s: &str| s.trim().to_lowercase();
    let h = rows.iter().position(|r| r.iter().any(|c| norm(c) == "receipt date") && r.iter().any(|c| norm(c) == "customer"))
        .ok_or_else(|| anyhow!("no header row with 'Receipt date' and 'Customer' – is this a receipts register?"))?;
    let col = |names: &[&str]| rows[h].iter().position(|c| names.contains(&norm(c).as_str()));
    let c_date = col(&["receipt date"]).unwrap();
    let c_cust = col(&["customer"]).unwrap();
    let c_amt = col(&["base amount", "amount", "receipt amount"]).ok_or_else(|| anyhow!("no 'Base amount' column"))?;
    let (c_method, c_ref) = (col(&["payment method"]), col(&["transaction number", "reference"]));

    let mut out = Vec::new();
    for r in &rows[h + 1..] {
        let get = |i: usize| r.get(i).map(|s| s.trim()).unwrap_or("");
        let cust = get(c_cust);
        let Some(date) = parse_date(get(c_date)) else { continue }; // skips the Total row and blanks
        if cust.is_empty() { continue; }
        let (ext, name) = cust.split_once("--").map(|(a, b)| (a.trim(), b.trim())).unwrap_or((cust, ""));
        let Some(amount) = parse_amount(get(c_amt)) else { continue };
        out.push(Receipt { ext_id: ext.to_string(), name: name.to_string(), date, amount,
            method: c_method.map(get).unwrap_or("").to_string(), reference: c_ref.map(get).unwrap_or("").to_string() });
    }
    if out.is_empty() { bail!("no receipt rows found"); }
    Ok(out)
}

pub async fn apply(tx: &mut Tx, receipts: &[Receipt], file: &str) -> Result<ReceiptResult> {
    let mut res = ReceiptResult { file: file.to_string(), receipts: receipts.len(), ..Default::default() };
    res.total = (receipts.iter().map(|r| r.amount).sum::<f64>() * 100.0).round() / 100.0;
    res.first_date = receipts.iter().map(|r| r.date).min().map(|d| d.to_string());
    res.last_date = receipts.iter().map(|r| r.date).max().map(|d| d.to_string());

    // customer -> date -> (total, count)
    let mut by_cust: HashMap<&str, BTreeMap<NaiveDate, (f64, usize)>> = HashMap::new();
    for r in receipts {
        let e = by_cust.entry(r.ext_id.as_str()).or_default().entry(r.date).or_insert((0.0, 0));
        e.0 += r.amount;
        e.1 += 1;
    }
    res.customers_in_file = by_cust.len();

    let mut ids = Vec::new();
    for n in [PAY_DATE, PAY_AMOUNT] {
        let id: Option<i64> = sqlx::query_scalar("SELECT id FROM attributes WHERE name=$1").bind(n).fetch_optional(&mut **tx).await?;
        ids.push(id.ok_or_else(|| anyhow!("the global attribute '{n}' is missing – add it under Attributes & companies"))?);
    }
    let (date_id, amt_id) = (ids[0], ids[1]);

    let mut keys: Vec<&&str> = by_cust.keys().collect();
    keys.sort();
    for ext in keys {
        let days = &by_cust[*ext];
        // Most recent day with a positive total (a day that nets to zero or less isn't a payment).
        let Some((date, (amt, n))) = days.iter().rev().find(|(_, (a, _))| *a > 0.005) else { continue };
        let found: Vec<(i64, i64, String, String)> = sqlx::query_as("SELECT id, company_id, status, name FROM customers WHERE ext_id=$1")
            .bind(*ext).fetch_all(&mut **tx).await?;
        let (cid, company_id, status, name) = match found.len() {
            0 => { res.not_in_app += 1; continue; }
            1 => found.into_iter().next().unwrap(),
            _ => { res.ambiguous.push(ext.to_string()); continue; }
        };
        if status == "active" { res.matched_open += 1 } else { res.matched_archived += 1 }

        // Keep every receipt for this customer (dashboard "collected"); the same file again adds nothing.
        for r in receipts.iter().filter(|r| r.ext_id == **ext) {
            res.receipts_stored += crate::exec!(&mut **tx, "INSERT INTO receipts (customer_id, receipt_date, amount, method, reference, source_file)
                VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING", cid, r.date, r.amount, &r.method, &r.reference, file) as usize;
        }

        let date_s = date.to_string();
        let amt_s = format!("{:.2}", amt);
        let cur_date: Option<String> = sqlx::query_scalar("SELECT value FROM attr_values WHERE customer_id=$1 AND attribute_id=$2")
            .bind(cid).bind(date_id).fetch_optional(&mut **tx).await?;
        let cur_amt: Option<String> = sqlx::query_scalar("SELECT value FROM attr_values WHERE customer_id=$1 AND attribute_id=$2")
            .bind(cid).bind(amt_id).fetch_optional(&mut **tx).await?;
        match cur_date.as_deref() {
            Some(d) if d > date_s.as_str() => { res.kept_newer += 1; continue; }
            Some(d) if d == date_s && cur_amt.as_deref().and_then(|a| a.parse::<f64>().ok()).is_some_and(|a| (a - amt).abs() < 0.005) => {
                res.already_current += 1;
                continue;
            }
            _ => {}
        }
        for (aid, v) in [(date_id, &date_s), (amt_id, &amt_s)] {
            crate::exec!(&mut **tx, "INSERT INTO attr_values (customer_id, attribute_id, value, updated_by) VALUES ($1,$2,$3,'receipts import')
                ON CONFLICT (customer_id, attribute_id) DO UPDATE SET value=excluded.value,
                updated_by=excluded.updated_by, updated_at=now()", cid, aid, v);
            crate::exec!(&mut **tx, "INSERT INTO company_attributes (company_id, attribute_id, enabled) VALUES ($1,$2,true)
                ON CONFLICT (company_id, attribute_id) DO UPDATE SET enabled=true", company_id, aid);
        }
        res.updated += 1;
        if res.examples.len() < 15 && *n > 1 {
            res.examples.push((name, date_s, (amt * 100.0).round() / 100.0, *n));
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_day_receipts_are_totalled_and_newer_values_kept() {
        let csv = "Receipt date,Customer,Payment method,Payment ID,Transaction number,Transaction date,Base amount,Base amount applied,Deposited on,Bank account,,\n\
11/10/2026,\"SL_1--Smith, Inc\",Check,,1,11/10/2026,100.00,100.00,Not Deposited,\n\
11/10/2026,\"SL_1--Smith, Inc\",Check,,2,11/10/2026,\"1,250.50\",0,Not Deposited,\n\
10/01/2026,\"SL_1--Smith, Inc\",Check,,3,10/01/2026,999.00,0,Not Deposited,\n\
11/01/2026,SL_2--Jones,Cash,,4,11/01/2026,50.00,0,Not Deposited,\n\
11/01/2026,SL_9--Not In App,Cash,,5,11/01/2026,75.00,0,Not Deposited,\n\
,,,,,Total,2474.50,100.00,,,,,\n";
        let rs = parse(csv.as_bytes().to_vec(), "r.csv").unwrap();
        assert_eq!(rs.len(), 5);
        assert_eq!(rs[0].name, "Smith, Inc");

        let (pool, schema) = crate::db::test_pool().await;
        let mut tx = crate::db::begin(&pool, "T").await.unwrap();
        let co = crate::db::ensure_company(&mut tx, "1001", "Sample Co", "").await.unwrap();
        for (ext, name) in [("SL_1", "Smith"), ("SL_2", "Jones")] {
            sqlx::query("INSERT INTO customers (company_id, ext_id, name, first_seen, last_seen) VALUES ($1,$2,$3,'2026-11-11','2026-11-11')")
                .bind(co).bind(ext).bind(name).execute(&mut *tx).await.unwrap();
        }
        // Jones already has a newer payment entered by hand.
        sqlx::query("INSERT INTO attr_values (customer_id, attribute_id, value) SELECT c.id, a.id, '2026-11-05' FROM customers c, attributes a
                     WHERE c.ext_id='SL_2' AND a.name=$1").bind(PAY_DATE).execute(&mut *tx).await.unwrap();

        let r = apply(&mut tx, &rs, "r.csv").await.unwrap();
        assert_eq!((r.customers_in_file, r.matched_open, r.updated, r.kept_newer, r.not_in_app), (3, 2, 1, 1, 1));
        assert_eq!(r.receipts_stored, 4); // SL_1 x3 and SL_2 x1; SL_9 is not in the app
        for (n, want) in [(PAY_DATE, "2026-11-10"), (PAY_AMOUNT, "1350.50")] {
            let v: String = sqlx::query_scalar("SELECT v.value FROM attr_values v JOIN attributes a ON a.id=v.attribute_id
                JOIN customers c ON c.id=v.customer_id WHERE c.ext_id='SL_1' AND a.name=$1").bind(n).fetch_one(&mut *tx).await.unwrap();
            assert_eq!(v, want);
        }

        // Re-importing the same file changes nothing.
        let r2 = apply(&mut tx, &rs, "r.csv").await.unwrap();
        assert_eq!((r2.updated, r2.already_current), (0, 1));
        assert_eq!(r2.receipts_stored, 0);
        tx.commit().await.unwrap();
        crate::db::drop_test_schema(pool, &schema).await;
    }
}
