//! Week-over-week dashboard: AR from the weekly snapshots, collections from stored receipts,
//! and contacts from the activity log, for one company or all the companies a viewer can see.

use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::collections::HashMap;

const BUCKETS: [&str; 7] = ["b0", "b30", "b60", "b90", "b182", "b365", "b366"];

fn monday(d: NaiveDate) -> NaiveDate { d - Duration::days(d.weekday().num_days_from_monday() as i64) }
fn f(v: &Value, k: &str) -> f64 { v[k].as_f64().unwrap_or(0.0) }
fn date(v: &Value, k: &str) -> NaiveDate { NaiveDate::parse_from_str(v[k].as_str().unwrap_or(""), "%Y-%m-%d").unwrap_or_default() }
fn round2(x: f64) -> f64 { (x * 100.0).round() / 100.0 }

/// `allowed` = companies the viewer may see (None = all); `company` = one company or None for the roll-up.
pub async fn build(pool: &PgPool, allowed: Option<Vec<i64>>, company: Option<i64>, weeks: usize, today: NaiveDate) -> Result<Value> {
    let companies = crate::rows!(pool, "SELECT id, code, name FROM companies
        WHERE ($1::bigint[] IS NULL OR id = ANY($1)) AND ($2::bigint IS NULL OR id = $2) ORDER BY code", &allowed, company);
    let ids: Vec<i64> = companies.iter().filter_map(|c| c["id"].as_i64()).collect();

    // One row per snapshot (import): bucket totals, plus what changed since that company's previous snapshot.
    let snaps = crate::rows!(pool, "
        WITH imp AS (
            SELECT i.id, i.company_id, i.as_of, p.id AS prev_id, COUNT(b.*) AS customers,
                   SUM(b.b0) AS b0, SUM(b.b30) AS b30, SUM(b.b60) AS b60, SUM(b.b90) AS b90,
                   SUM(b.b182) AS b182, SUM(b.b365) AS b365, SUM(b.b366) AS b366, SUM(b.total) AS total,
                   SUM(b.b182 + b.b365 + b.b366) AS over90
            FROM imports i JOIN balances b ON b.import_id = i.id
            LEFT JOIN LATERAL (SELECT id FROM imports p WHERE p.company_id = i.company_id AND p.as_of < i.as_of
                               ORDER BY p.as_of DESC LIMIT 1) p ON true
            WHERE i.company_id = ANY($1)
            GROUP BY i.id, p.id
        )
        SELECT imp.*,
          CASE WHEN prev_id IS NULL THEN 0 ELSE (SELECT COUNT(*) FROM balances bp WHERE bp.import_id = imp.prev_id
               AND NOT EXISTS (SELECT 1 FROM balances bc WHERE bc.import_id = imp.id AND bc.customer_id = bp.customer_id)) END AS paid_off_count,
          CASE WHEN prev_id IS NULL THEN 0 ELSE (SELECT COALESCE(SUM(bp.total), 0) FROM balances bp WHERE bp.import_id = imp.prev_id
               AND NOT EXISTS (SELECT 1 FROM balances bc WHERE bc.import_id = imp.id AND bc.customer_id = bp.customer_id)) END AS paid_off_amt,
          CASE WHEN prev_id IS NULL THEN 0 ELSE (SELECT COUNT(*) FROM balances bc WHERE bc.import_id = imp.id
               AND NOT EXISTS (SELECT 1 FROM balances bp WHERE bp.import_id = imp.prev_id AND bp.customer_id = bc.customer_id)) END AS new_count,
          CASE WHEN prev_id IS NULL THEN 0 ELSE (SELECT COALESCE(SUM(bc.total), 0) FROM balances bc WHERE bc.import_id = imp.id
               AND NOT EXISTS (SELECT 1 FROM balances bp WHERE bp.import_id = imp.prev_id AND bp.customer_id = bc.customer_id)) END AS new_amt,
          -- 90+ dollars that weren't 90+ for that customer on the previous snapshot
          CASE WHEN prev_id IS NULL THEN 0 ELSE (SELECT COALESCE(SUM(GREATEST(0, (bc.b182 + bc.b365 + bc.b366)
                  - COALESCE(bp.b182 + bp.b365 + bp.b366, 0))), 0)
               FROM balances bc LEFT JOIN balances bp ON bp.import_id = imp.prev_id AND bp.customer_id = bc.customer_id
               WHERE bc.import_id = imp.id) END AS moved90
        FROM imp ORDER BY company_id, as_of", &ids);

    let contact_kinds: Vec<String> = crate::db::CONTACT_KINDS.iter().map(|s| s.to_string()).collect();
    let receipts = crate::rows!(pool, "
        SELECT c.company_id, date_trunc('week', r.receipt_date)::date AS week, SUM(r.amount) AS collected, COUNT(*) AS receipts
        FROM receipts r JOIN customers c ON c.id = r.customer_id
        WHERE c.company_id = ANY($1) GROUP BY 1, 2", &ids);
    let acts = crate::rows!(pool, "
        SELECT c.company_id, date_trunc('week', a.entry_date)::date AS week,
               COUNT(*) FILTER (WHERE a.kind = ANY($2)) AS contacts, COUNT(*) AS entries
        FROM activities a JOIN customers c ON c.id = a.customer_id
        WHERE a.deleted_at IS NULL AND c.company_id = ANY($1) GROUP BY 1, 2", &ids, &contact_kinds);

    // (company, week) -> value
    let mut coll: HashMap<(i64, NaiveDate), (f64, i64)> = HashMap::new();
    for r in &receipts { coll.insert((r["company_id"].as_i64().unwrap_or_default(), date(r, "week")), (f(r, "collected"), r["receipts"].as_i64().unwrap_or(0))); }
    let mut act: HashMap<(i64, NaiveDate), (i64, i64)> = HashMap::new();
    for r in &acts { act.insert((r["company_id"].as_i64().unwrap_or_default(), date(r, "week")), (r["contacts"].as_i64().unwrap_or(0), r["entries"].as_i64().unwrap_or(0))); }
    let mut by_co: HashMap<i64, Vec<&Value>> = HashMap::new();
    for s in &snaps { by_co.entry(s["company_id"].as_i64().unwrap_or_default()).or_default().push(s); }

    // Weekly axis: the last `weeks` Mondays up to this week.
    let this_week = monday(today);
    let axis: Vec<NaiveDate> = (0..weeks).rev().map(|i| this_week - Duration::days(7 * i as i64)).collect();

    let mut week_rows = Vec::new();
    for &w in &axis {
        let mut row = Map::new();
        let mut sums = [0.0f64; 7];
        let (mut total, mut over90, mut customers) = (0.0, 0.0, 0i64);
        let (mut paid_n, mut paid_amt, mut new_n, mut new_amt, mut moved90) = (0i64, 0.0, 0i64, 0.0, 0.0);
        let (mut reporting, mut carried, mut missing) = (0, 0, 0);
        let (mut collected, mut n_receipts, mut contacts, mut entries) = (0.0, 0i64, 0i64, 0i64);
        for &cid in &ids {
            let list = by_co.get(&cid).map(|v| v.as_slice()).unwrap_or(&[]);
            // Latest snapshot on or before this week (carried forward if the company didn't report this week).
            match list.iter().rev().find(|s| monday(date(s, "as_of")) <= w) {
                Some(s) => {
                    for (i, b) in BUCKETS.iter().enumerate() { sums[i] += f(s, b); }
                    total += f(s, "total");
                    over90 += f(s, "over90");
                    customers += s["customers"].as_i64().unwrap_or(0);
                    if monday(date(s, "as_of")) == w { reporting += 1 } else { carried += 1 }
                }
                None => missing += 1,
            }
            // Changes reported during this week (every snapshot taken that week).
            for s in list.iter().filter(|s| monday(date(s, "as_of")) == w) {
                paid_n += s["paid_off_count"].as_i64().unwrap_or(0);
                paid_amt += f(s, "paid_off_amt");
                new_n += s["new_count"].as_i64().unwrap_or(0);
                new_amt += f(s, "new_amt");
                moved90 += f(s, "moved90");
            }
            if let Some((c, n)) = coll.get(&(cid, w)) { collected += c; n_receipts += n; }
            if let Some((c, e)) = act.get(&(cid, w)) { contacts += c; entries += e; }
        }
        row.insert("week".into(), json!(w.to_string()));
        for (i, b) in BUCKETS.iter().enumerate() { row.insert((*b).into(), json!(round2(sums[i]))); }
        let has_ar = reporting + carried > 0;
        row.insert("has_ar".into(), json!(has_ar));
        row.insert("total".into(), json!(round2(total)));
        row.insert("over90".into(), json!(round2(over90)));
        row.insert("pct90".into(), if has_ar && total.abs() > 0.005 { json!((over90 / total * 1000.0).round() / 10.0) } else { Value::Null });
        row.insert("customers".into(), json!(customers));
        row.insert("paid_off_count".into(), json!(paid_n));
        row.insert("paid_off_amt".into(), json!(round2(paid_amt)));
        row.insert("new_count".into(), json!(new_n));
        row.insert("new_amt".into(), json!(round2(new_amt)));
        row.insert("moved90".into(), json!(round2(moved90)));
        row.insert("reporting".into(), json!(reporting));
        row.insert("carried".into(), json!(carried));
        row.insert("missing".into(), json!(missing));
        row.insert("collected".into(), json!(round2(collected)));
        row.insert("receipts".into(), json!(n_receipts));
        row.insert("contacts".into(), json!(contacts));
        row.insert("entries".into(), json!(entries));
        week_rows.push(Value::Object(row));
    }

    // Focus week: the latest week that has a fresh AR report in scope (else this week).
    let focus = axis.iter().rev().copied().find(|w| ids.iter().any(|cid|
        by_co.get(cid).is_some_and(|l| l.iter().any(|s| monday(date(s, "as_of")) == *w)))).unwrap_or(this_week);

    // Per-company table for the focus week.
    let mut by_company = Vec::new();
    for c in &companies {
        let cid = c["id"].as_i64().unwrap_or_default();
        let list = by_co.get(&cid).map(|v| v.as_slice()).unwrap_or(&[]);
        let cur = list.iter().rev().find(|s| monday(date(s, "as_of")) <= focus);
        let prev = cur.and_then(|s| s["prev_id"].as_i64()).and_then(|pid| list.iter().find(|x| x["id"].as_i64() == Some(pid)));
        let fresh = cur.is_some_and(|s| monday(date(s, "as_of")) == focus);
        let (cl, _) = coll.get(&(cid, focus)).copied().unwrap_or((0.0, 0));
        let (ct, _) = act.get(&(cid, focus)).copied().unwrap_or((0, 0));
        by_company.push(json!({
            "id": cid, "code": c["code"], "name": c["name"],
            "as_of": cur.map(|s| s["as_of"].clone()), "fresh": fresh,
            "total": cur.map(|s| f(s, "total")), "prev_total": prev.map(|s| f(s, "total")),
            "over90": cur.map(|s| f(s, "over90")), "prev_over90": prev.map(|s| f(s, "over90")),
            "customers": cur.map(|s| s["customers"].clone()),
            "paid_off_count": if fresh { cur.map(|s| s["paid_off_count"].clone()) } else { None },
            "paid_off_amt": if fresh { cur.map(|s| f(s, "paid_off_amt")) } else { None },
            "new_count": if fresh { cur.map(|s| s["new_count"].clone()) } else { None },
            "moved90": if fresh { cur.map(|s| f(s, "moved90")) } else { None },
            "collected": round2(cl), "contacts": ct,
        }));
    }

    Ok(json!({ "companies": companies, "weeks": week_rows, "focus_week": focus.to_string(), "this_week": this_week.to_string(),
               "by_company": by_company, "has_receipts": !receipts.is_empty() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{parse_workbook, apply_sheet, Line, SheetData};

    /// Path to a real customer aging report, from `AR_SAMPLE_REPORT`. The test is skipped when it is not set.
    fn sample() -> Option<Vec<u8>> { std::fs::read(std::env::var("AR_SAMPLE_REPORT").ok()?).ok() }

    #[tokio::test]
    async fn two_weeks_of_snapshots() {
        let Some(bytes) = sample() else { eprintln!("AR_SAMPLE_REPORT not set; skipping"); return };
        let (pool, schema) = crate::db::test_pool().await;
        let mut sheets = parse_workbook(bytes).unwrap();
        let sd = sheets.iter_mut().find(|s| !s.lines.is_empty()).unwrap();
        let copy = |l: &Line| Line { ext_id: l.ext_id.clone(), name: l.name.clone(), posting: l.posting, src: l.src.clone(), bucket: l.bucket, amount: l.amount };
        let apply = |sd: SheetData| { let pool = pool.clone(); async move {
            let mut tx = crate::db::begin(&pool, "T").await.unwrap();
            apply_sheet(&mut tx, &sd, "t.xlsx", "T").await.unwrap();
            tx.commit().await.unwrap();
        }};
        let mk = |sd: &SheetData, lines: Vec<Line>, as_of: NaiveDate| SheetData { sheet: sd.sheet.clone(), code: sd.code.clone(), location: sd.location.clone(),
            legal_name: sd.legal_name.clone(), as_of, lines, grand_total: None };

        // Week 1 = the report minus one customer moved back to 61-90; week 2 = the real report minus one paid-off customer.
        let wk2 = sd.as_of;                      // 2026-11-11
        let wk1 = wk2 - Duration::days(7);
        let gone = sd.lines[0].ext_id.clone();   // in week 1, not in week 2 (paid off)
        let gone_total: f64 = sd.lines.iter().filter(|l| l.ext_id == gone).map(|l| l.amount).sum();
        let mover = sd.lines.iter().find(|l| l.bucket == 4 && l.ext_id != gone).unwrap().ext_id.clone();
        let mover_amt: f64 = sd.lines.iter().filter(|l| l.ext_id == mover && l.bucket == 4).map(|l| l.amount).sum();
        let w1_lines: Vec<Line> = sd.lines.iter().map(|l| { let mut c = copy(l); if c.ext_id == mover && c.bucket == 4 { c.bucket = 3; } c }).collect();
        let w2_lines: Vec<Line> = sd.lines.iter().filter(|l| l.ext_id != gone).map(copy).collect();
        apply(mk(sd, w1_lines, wk1)).await;
        apply(mk(sd, w2_lines, wk2)).await;

        let d = build(&pool, None, None, 4, wk2).await.unwrap();
        let weeks = d["weeks"].as_array().unwrap();
        let (w1, w2) = (&weeks[2], &weeks[3]);
        assert_eq!(d["focus_week"], json!(monday(wk2).to_string()));
        assert_eq!(w2["paid_off_count"], json!(1));
        assert!((f(w2, "paid_off_amt") - gone_total).abs() < 0.01);
        assert!((f(w2, "moved90") - mover_amt).abs() < 0.01, "moved90 {} vs {}", f(w2, "moved90"), mover_amt);
        assert!((f(w1, "total") - f(w2, "total") - gone_total).abs() < 0.01);
        assert_eq!(w1["reporting"], json!(1));
        // Before any report: no AR, and nothing carried.
        assert_eq!(weeks[0]["has_ar"], json!(false));
        crate::db::drop_test_schema(pool, &schema).await;
    }
}
