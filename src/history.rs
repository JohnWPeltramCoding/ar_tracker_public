//! One-time (repeatable) import of the existing formatted Excel tracking workbooks:
//! weekly notes columns, extra attribute columns, and colour-coded statuses.
//! Existing notes and attribute values in the app are never overwritten.

use anyhow::{anyhow, Context, Result};
use calamine::{open_workbook_auto_from_rs, Data, Reader};
use chrono::{Datelike, Duration, NaiveDate};
use quick_xml::events::{BytesStart, Event};
use regex::Regex;
use crate::db::Tx;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read};

#[derive(Serialize, Default, Debug)]
pub struct HistoryResult {
    pub file: String,
    pub company: String,
    pub as_of: String,
    pub sheets: Vec<String>,
    pub customers_matched: usize,
    pub customers_created_archived: usize,
    pub notes_imported: usize,
    pub notes_kept_existing: usize,
    pub attrs_set: usize,
    pub statuses_set: usize,
    pub new_attributes: Vec<String>,
    pub note_columns: Vec<(String, String)>, // (header, week_of)
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------- cell fills (xlsx XML)

type Fills = HashMap<String, HashMap<(u32, u32), String>>; // sheet name -> (row, col) -> colour key

fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.local_name().as_ref() == key).map(|a| {
        a.value.to_string()
            .replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
    })
}

fn zip_text(z: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<String> {
    let mut f = z.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

fn cell_ref(r: &str) -> Option<(u32, u32)> {
    let mut col = 0u32;
    let mut digits = String::new();
    for ch in r.chars() {
        if ch.is_ascii_alphabetic() {
            col = col * 26 + (ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
        } else {
            digits.push(ch);
        }
    }
    Some((digits.parse::<u32>().ok()? - 1, col.checked_sub(1)?))
}

/// Solid background colour of every styled cell, keyed like "FF00B0F0" or "theme8:0.6".
fn read_fills(bytes: &[u8]) -> Fills {
    let mut out = Fills::new();
    let Ok(mut z) = zip::ZipArchive::new(Cursor::new(bytes)) else { return out };

    // styles.xml: fills[] then cellXfs[] -> fill colour
    let mut fills: Vec<Option<String>> = Vec::new();
    let mut xf_fill: Vec<Option<String>> = Vec::new();
    if let Some(s) = zip_text(&mut z, "xl/styles.xml") {
        let mut rd = quick_xml::Reader::from_str(&s);
        let (mut in_fills, mut in_xfs, mut solid) = (false, false, false);
        let mut cur: Option<String> = None;
        loop {
            match rd.read_event() {
                Ok(ev @ (Event::Start(_) | Event::Empty(_))) => { let (Event::Start(e) | Event::Empty(e)) = &ev else { unreachable!() }; let empty = matches!(ev, Event::Empty(_)); match e.local_name().as_ref() {
                    "fills" => in_fills = true,
                    "cellXfs" => in_xfs = true,
                    "fill" if in_fills => { cur = None; solid = false; if empty { fills.push(None); } }
                    "patternFill" if in_fills => solid = attr(&e, "patternType").as_deref() == Some("solid"),
                    "fgColor" if in_fills && solid => {
                        cur = attr(&e, "rgb").or_else(|| attr(&e, "theme").map(|t| format!("theme{t}:{}", attr(&e, "tint").unwrap_or_default().chars().take(4).collect::<String>())))
                            .or_else(|| attr(&e, "indexed").map(|i| format!("idx{i}")));
                    }
                    "xf" if in_xfs => {
                        let id: usize = attr(&e, "fillId").and_then(|v| v.parse().ok()).unwrap_or(0);
                        xf_fill.push(fills.get(id).cloned().flatten());
                    }
                    _ => {}
                }},
                Ok(Event::End(e)) => match e.local_name().as_ref() {
                    "fills" => in_fills = false,
                    "cellXfs" => in_xfs = false,
                    "fill" if in_fills => fills.push(cur.take()),
                    _ => {}
                },
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }

    // workbook.xml + rels: sheet name -> worksheet part
    let mut rid_to_target = HashMap::new();
    if let Some(s) = zip_text(&mut z, "xl/_rels/workbook.xml.rels") {
        let mut rd = quick_xml::Reader::from_str(&s);
        loop {
            match rd.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == "Relationship" => {
                    if let (Some(id), Some(t)) = (attr(&e, "Id"), attr(&e, "Target")) {
                        let t = t.trim_start_matches('/').to_string();
                        rid_to_target.insert(id, if t.starts_with("xl/") { t } else { format!("xl/{t}") });
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }
    let mut sheets = Vec::new();
    if let Some(s) = zip_text(&mut z, "xl/workbook.xml") {
        let mut rd = quick_xml::Reader::from_str(&s);
        loop {
            match rd.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == "sheet" => {
                    if let (Some(n), Some(id)) = (attr(&e, "name"), attr(&e, "id")) {
                        if let Some(t) = rid_to_target.get(&id) { sheets.push((n, t.clone())); }
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }
    for (name, part) in sheets {
        let Some(s) = zip_text(&mut z, &part) else { continue };
        let mut m = HashMap::new();
        let mut rd = quick_xml::Reader::from_str(&s);
        loop {
            match rd.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == "c" => {
                    let (Some(r), Some(st)) = (attr(&e, "r"), attr(&e, "s")) else { continue };
                    let Ok(st) = st.parse::<usize>() else { continue };
                    if let (Some(pos), Some(Some(fill))) = (cell_ref(&r), xf_fill.get(st)) {
                        m.insert(pos, fill.clone());
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        out.insert(name, m);
    }
    out
}

fn fill_to_hex(k: &str) -> String {
    if k.len() == 8 && k.chars().all(|c| c.is_ascii_hexdigit()) { format!("#{}", &k[2..]) } else { "#d9d9d9".into() }
}

// ---------------------------------------------------------------- helpers

fn s(c: &Data) -> String {
    match c {
        Data::Empty => String::new(),
        Data::String(x) => x.trim().to_string(),
        Data::DateTime(d) => excel_date(d.as_f64()).map(|d| d.format("%m/%d/%Y").to_string()).unwrap_or_default(),
        Data::Float(f) => { let r = (f * 100.0).round() / 100.0; if r.fract() == 0.0 { format!("{r:.0}") } else { format!("{r}") } }
        o => o.to_string().trim().to_string(),
    }
}
fn excel_date(serial: f64) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(1899, 12, 30)?.checked_add_signed(Duration::days(serial.floor() as i64))
}
fn as_date(c: &Data) -> Option<NaiveDate> {
    match c {
        Data::DateTime(d) => excel_date(d.as_f64()),
        Data::String(x) => ["%m/%d/%Y", "%m/%d/%y", "%Y-%m-%d"].iter().find_map(|f| NaiveDate::parse_from_str(x.trim(), f).ok()),
        _ => None,
    }
}
fn monday(d: NaiveDate) -> NaiveDate { d - Duration::days(d.weekday().num_days_from_monday() as i64) }

/// "Week of 11/9 Notes", "Wk of 9/29", "9.17.2026 Meeting Notes", "wk of 31 Aug Notes" -> Monday of that week.
pub fn week_from_header(h: &str, as_of: NaiveDate) -> Option<NaiveDate> {
    let num = Regex::new(r"(\d{1,2})[/.](\d{1,2})(?:[/.](\d{2,4}))?").unwrap();
    let months = "jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec";
    let dm = Regex::new(&format!(r"(?i)\b(\d{{1,2}})\s*({months})[a-z]*")).unwrap();
    let md = Regex::new(&format!(r"(?i)\b({months})[a-z]*\.?\s*(\d{{1,2}})\b")).unwrap();
    let mon = |m: &str| months.split('|').position(|x| m.to_lowercase().starts_with(x)).map(|i| i as u32 + 1);
    let (m, d, y): (u32, u32, Option<i32>) = if let Some(c) = num.captures(h) {
        (c[1].parse().ok()?, c[2].parse().ok()?, c.get(3).and_then(|y| y.as_str().parse::<i32>().ok()).map(|y| if y < 100 { y + 2000 } else { y }))
    } else if let Some(c) = dm.captures(h) {
        (mon(&c[2])?, c[1].parse().ok()?, None)
    } else if let Some(c) = md.captures(h) {
        (mon(&c[1])?, c[2].parse().ok()?, None)
    } else {
        return None;
    };
    let date = match y {
        Some(y) => NaiveDate::from_ymd_opt(y, m, d)?,
        None => {
            let d0 = NaiveDate::from_ymd_opt(as_of.year(), m, d)?;
            if d0 > as_of + Duration::days(120) { NaiveDate::from_ymd_opt(as_of.year() - 1, m, d)? } else { d0 }
        }
    };
    Some(monday(date))
}

fn norm(h: &str) -> String {
    let paren = Regex::new(r"\(.*?\)").unwrap();
    paren.replace_all(&h.to_lowercase(), " ").replace("referal", "referral")
        .split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty() && !["actual", "customer", "enter"].contains(w))
        .collect::<Vec<_>>().join(" ")
}

const ALIASES: &[(&str, &str)] = &[
    ("lien letter", "Lien Letter Sent"),
    ("lien filed", "Lien Filed"),
    ("apr referral", "APR Referral Submitted"),
    ("last payment date", "Last Known Payment Date"),
    ("last payment amount", "Last Known Payment Amount"),
    ("contact date", "Most Recent Contact Date"),
    ("contact method", "Most Recent Contact Method"),
];

fn is_bucket(h: &str) -> bool {
    let t = h.trim().trim_end_matches('+');
    t.split_once('-').is_some_and(|(a, b)| (a.is_empty() || a.trim().parse::<i64>().is_ok()) && (b.is_empty() || b.trim().parse::<i64>().is_ok()))
}

enum Col { Skip, Id, Name, Note(NaiveDate, bool /* week parsed from header */), Attr(String) }

// ---------------------------------------------------------------- import

pub async fn import_history(tx: &mut Tx, bytes: Vec<u8>, file: &str, user: &str) -> Result<HistoryResult> {
    let fills = read_fills(&bytes);
    let mut res = HistoryResult { file: file.to_string(), ..Default::default() };

    // Load every sheet first; find the company and as-of date from whichever sheet has them.
    let sheets: Vec<(String, calamine::Range<Data>)> = {
        let mut wb = open_workbook_auto_from_rs(Cursor::new(bytes)).context("not a readable Excel file")?;
        let mut v = Vec::new();
        for name in wb.sheet_names().to_owned() {
            let r = wb.worksheet_range(&name)?;
            v.push((name, r));
        }
        v
    };
    let mut location = None;
    let mut as_of = None;
    let mut legal = String::new();
    for (_, r) in &sheets {
        for row in r.rows().take(15) {
            for (i, c) in row.iter().enumerate() {
                let label = s(c);
                let rest = || row.iter().skip(i + 1).find(|x| !matches!(x, Data::Empty));
                match label.as_str() {
                    "Location:" if location.is_none() => location = rest().map(s),
                    "As of date:" if as_of.is_none() => as_of = rest().and_then(as_date),
                    "Company name:" if legal.is_empty() => legal = rest().map(s).unwrap_or_default(),
                    _ if label.starts_with("As of date:") && as_of.is_none() => as_of = as_date(&Data::String(label[11..].trim().to_string())),
                    _ => {}
                }
            }
        }
    }
    let location = location.ok_or_else(|| anyhow!("no 'Location:' found – can't tell which company this workbook belongs to"))?;
    let as_of = as_of.ok_or_else(|| anyhow!("no 'As of date:' found"))?;
    let (code, loc_name) = location.split_once("--").map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .unwrap_or((location.clone(), location.clone()));
    res.company = format!("{code} – {loc_name}");
    res.as_of = as_of.to_string();
    let fallback_week = monday(as_of);
    let author = if user.is_empty() { "Excel import".to_string() } else { format!("Excel import ({user})") };

    let company_id = crate::db::ensure_company(tx, &code, &loc_name, &legal).await?;
    let mut attrs: HashMap<String, (i64, String, String)> = HashMap::new(); // norm(name) -> (id, name, kind)
    for (id, n, k) in sqlx::query_as::<_, (i64, String, String)>("SELECT id, name, kind FROM attributes").fetch_all(&mut **tx).await? {
        attrs.insert(norm(&n), (id, n, k));
    }
    let status_id = attrs.get("status").map(|a| a.0);
    let mut cust_cache: HashMap<String, i64> = HashMap::new();
    let ws = Regex::new(r"\s+").unwrap();

    for (sheet, range) in &sheets {
        let (r0, c0) = range.start().unwrap_or((0, 0));
        let rows: Vec<&[Data]> = range.rows().collect();
        let Some(h) = rows.iter().position(|r| r.iter().any(|c| s(c).eq_ignore_ascii_case("Customer ID"))) else { continue };
        let header: Vec<String> = rows[h].iter().map(s).collect();
        if header.iter().any(|x| x.eq_ignore_ascii_case("Document no.")) {
            continue; // invoice-level detail, not a tracking sheet
        }
        res.sheets.push(sheet.clone());
        let sheet_fills = fills.get(sheet);
        let fill_at = |r: usize, c: usize| sheet_fills.and_then(|m| m.get(&(r0 + r as u32, c0 + c as u32))).cloned();

        // Status colour legend above the header: an empty coloured cell followed by its label.
        let mut legend: HashMap<String, String> = HashMap::new();
        for (ri, row) in rows[..h].iter().enumerate() {
            for ci in 0..row.len().saturating_sub(1) {
                let label = s(&row[ci + 1]);
                if s(&row[ci]).is_empty() && !label.is_empty() && !label.to_lowercase().starts_with("top 25") {
                    if let Some(f) = fill_at(ri, ci) { legend.insert(f, label); }
                }
            }
        }

        // Classify columns.
        let mut cols = Vec::new();
        for hd in &header {
            let l = hd.to_lowercase();
            let col = if l == "customer id" { Col::Id }
                else if l == "customer name" { Col::Name }
                else if hd.is_empty() || is_bucket(hd) || ["total", "top 25 in", "tab", "reason"].contains(&l.as_str()) { Col::Skip }
                else if l.contains("note") {
                    match week_from_header(hd, as_of) { Some(w) => Col::Note(w, true), None => Col::Note(fallback_week, false) }
                } else { Col::Attr(hd.clone()) };
            if let Col::Note(w, ok) = &col {
                res.note_columns.push((format!("{sheet}: {hd}"), if *ok { w.to_string() } else { format!("{w} (no date in header – used report week)") }));
            }
            cols.push(col);
        }
        let Some(id_col) = cols.iter().position(|c| matches!(c, Col::Id)) else { continue };
        let name_col = cols.iter().position(|c| matches!(c, Col::Name));

        // Resolve attribute columns to (existing or new) global attributes.
        let mut attr_cols: Vec<(usize, i64, String)> = Vec::new();
        for (ci, c) in cols.iter().enumerate() {
            let Col::Attr(hd) = c else { continue };
            let values: Vec<&Data> = rows[h + 1..].iter().filter_map(|r| r.get(ci)).filter(|v| !s(v).is_empty()).collect();
            if values.is_empty() { continue; }
            let n = norm(hd);
            let found = attrs.get(&n).cloned().or_else(|| {
                ALIASES.iter().find(|(k, _)| n.contains(k)).and_then(|(_, target)| attrs.get(&norm(target)).cloned())
            });
            let (aid, kind) = match found {
                Some((id, _, k)) => (id, k),
                None => {
                    let kind = if values.iter().all(|v| as_date(v).is_some()) { "date" }
                        else if values.iter().all(|v| matches!(v, Data::Float(_) | Data::Int(_))) { "number" }
                        else if values.iter().any(|v| s(v).len() > 80) { "longtext" } else { "text" };
                    let name = ws.replace_all(hd.trim(), " ").to_string();
                    let id: i64 = sqlx::query_scalar("INSERT INTO attributes (name, kind, sort) VALUES ($1, $2, (SELECT COALESCE(MAX(sort),0)+10 FROM attributes)) RETURNING id")
                        .bind(&name).bind(kind).fetch_one(&mut **tx).await?;
                    attrs.insert(n.clone(), (id, name.clone(), kind.to_string()));
                    res.new_attributes.push(name);
                    (id, kind.to_string())
                }
            };
            crate::exec!(&mut **tx, "INSERT INTO company_attributes (company_id, attribute_id, enabled) VALUES ($1,$2,true)
                ON CONFLICT (company_id, attribute_id) DO UPDATE SET enabled=true", company_id, aid);
            attr_cols.push((ci, aid, kind));
        }
        if let (Some(sid), false) = (status_id, legend.is_empty()) {
            crate::exec!(&mut **tx, "INSERT INTO company_attributes (company_id, attribute_id, enabled) VALUES ($1,$2,true)
                ON CONFLICT (company_id, attribute_id) DO UPDATE SET enabled=true", company_id, sid);
        }

        for (ri, row) in rows.iter().enumerate().skip(h + 1) {
            let ext = row.get(id_col).map(s).unwrap_or_default();
            if ext.is_empty() || ext.contains(' ') { continue; }
            let name = name_col.and_then(|c| row.get(c)).map(s).unwrap_or_default();

            let cid = match cust_cache.get(&ext) {
                Some(id) => *id,
                None => {
                    let found: Option<i64> = sqlx::query_scalar("SELECT id FROM customers WHERE company_id=$1 AND ext_id=$2")
                        .bind(company_id).bind(&ext).fetch_optional(&mut **tx).await?;
                    let id = match found {
                        Some(id) => { res.customers_matched += 1; id }
                        None => {
                            // Not on any imported report: it has been paid off / cleared. Keep it in the archive.
                            res.customers_created_archived += 1;
                            sqlx::query_scalar("INSERT INTO customers (company_id, ext_id, name, status, first_seen, last_seen, archived_on)
                                VALUES ($1,$2,$3,'archived',$4,$4,$4) RETURNING id")
                                .bind(company_id).bind(&ext).bind(if name.is_empty() { &ext } else { &name }).bind(as_of)
                                .fetch_one(&mut **tx).await?
                        }
                    };
                    cust_cache.insert(ext.clone(), id);
                    id
                }
            };

            // Notes: combine columns that land in the same week.
            let mut weeks: BTreeMap<NaiveDate, Vec<String>> = BTreeMap::new();
            for (ci, c) in cols.iter().enumerate() {
                if let Col::Note(w, parsed) = c {
                    let v = row.get(ci).map(s).unwrap_or_default();
                    if v.is_empty() { continue; }
                    let v = if *parsed { v } else { format!("[{}] {v}", header[ci]) };
                    let e = weeks.entry(*w).or_default();
                    if !e.contains(&v) { e.push(v); }
                }
            }
            for (w, parts) in weeks {
                let body = parts.join("\n\n");
                // Already imported (e.g. the same workbook run twice, or last week's workbook)? Skip it.
                let exists: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM activities WHERE customer_id=$1 AND entry_date=$2 AND TRIM(body)=TRIM($3)")
                    .bind(cid).bind(w).bind(&body).fetch_optional(&mut **tx).await?;
                if exists.is_some() {
                    res.notes_kept_existing += 1;
                } else {
                    crate::exec!(&mut **tx, "INSERT INTO activities (customer_id, entry_date, kind, body, author, source) VALUES ($1,$2,'Note',$3,$4,'import')",
                        cid, w, &body, &author);
                    res.notes_imported += 1;
                }
            }

            // Attributes: fill only where the app has no value yet.
            for (ci, aid, kind) in &attr_cols {
                let Some(cell) = row.get(*ci) else { continue };
                let v = match kind.as_str() {
                    "date" => as_date(cell).map(|d| d.to_string()).unwrap_or_else(|| s(cell)),
                    "money" | "number" => match cell { Data::Float(f) => format!("{:.2}", f), Data::Int(i) => i.to_string(), o => s(o) },
                    _ => s(cell),
                };
                if v.is_empty() { continue; }
                res.attrs_set += crate::exec!(&mut **tx, "INSERT INTO attr_values (customer_id, attribute_id, value, updated_by) VALUES ($1,$2,$3,'import')
                    ON CONFLICT DO NOTHING", cid, aid, &v) as usize;
            }

            // Status from the colour of the Customer ID (or name) cell.
            if let (Some(sid), false) = (status_id, legend.is_empty()) {
                let fill = fill_at(ri, id_col).or_else(|| name_col.and_then(|c| fill_at(ri, c)));
                if let Some((f, label)) = fill.as_ref().and_then(|f| legend.get(f).map(|l| (f, l))) {
                    let opt = status_option(tx, sid, label, f).await?;
                    res.statuses_set += crate::exec!(&mut **tx, "INSERT INTO attr_values (customer_id, attribute_id, value, updated_by) VALUES ($1,$2,$3,'import')
                        ON CONFLICT DO NOTHING", cid, sid, &opt) as usize;
                }
            }
        }
    }
    if res.sheets.is_empty() {
        res.warnings.push("No sheet with a 'Customer ID' header was found.".into());
    }
    Ok(res)
}

/// Map a workbook legend label to a Status choice, adding a new choice if nothing fits.
async fn status_option(tx: &mut Tx, sid: i64, label: &str, fill: &str) -> Result<String> {
    let l = label.to_lowercase();
    let guess = if l.contains("payment plan") { Some("Payment Plan") }
        else if l.contains("collection") { Some("Requesting 3rd party collections") }
        else if l.contains("write off") || l.contains("write-off") { Some("Requesting write off") }
        else if l.contains("flag") || l.contains("attention") { Some("Flagged for attention") }
        else { None };
    let opts_s: String = sqlx::query_scalar("SELECT options FROM attributes WHERE id=$1").bind(sid).fetch_one(&mut **tx).await?;
    let mut opts: Vec<Value> = serde_json::from_str(&opts_s).unwrap_or_default();
    let want = guess.unwrap_or(label.trim());
    if let Some(o) = opts.iter().find(|o| o["label"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(want))) {
        return Ok(o["label"].as_str().unwrap().to_string());
    }
    opts.push(json!({"label": want, "color": fill_to_hex(fill)}));
    crate::exec!(&mut **tx, "UPDATE attributes SET options=$2 WHERE id=$1", sid, Value::Array(opts).to_string());
    Ok(want.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn week_headers() {
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
        let w = |h| week_from_header(h, as_of).map(|d| d.to_string());
        assert_eq!(w("Week of 11/16 Notes").as_deref(), Some("2026-11-16"));
        assert_eq!(w("Week of 11/9 Notes").as_deref(), Some("2026-11-09"));
        assert_eq!(w("Wk of 9/29 Notes").as_deref(), Some("2026-09-28"));
        assert_eq!(w("9.17.2026 Meeting Notes").as_deref(), Some("2026-09-14"));
        assert_eq!(w("wk of 31 Aug Notes (initials on EE commenting)").as_deref(), Some("2026-08-31"));
        assert_eq!(w("Notes for Week of 11/2 (Please initial and timestamp all notes regardless of file owner)").as_deref(), Some("2026-11-02"));
        assert_eq!(w("Latest note"), None);
    }
}
