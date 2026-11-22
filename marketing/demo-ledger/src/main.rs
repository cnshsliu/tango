//! ledger — a tiny personal ledger on SQLite.
//!   ledger add <amount> <category> [note]   record an expense (yuan, e.g. 38.5)
//!   ledger list [n]                         latest n entries (default 10)
//!   ledger month [YYYY-MM]                  totals per category for a month
//!   ledger serve [port]                     JSON over HTTP: GET /entries, GET /month
//! The database is ./ledger.db unless LEDGER_DB is set.
mod db;
mod serve;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = std::env::var("LEDGER_DB").unwrap_or_else(|_| "ledger.db".into());
    let conn = match db::open(&path) {
        Ok(c) => c,
        Err(e) => return fail(&format!("cannot open {path}: {e}")),
    };
    let r = match args.first().map(String::as_str) {
        Some("add") => cmd_add(&conn, &args[1..]),
        Some("list") => cmd_list(&conn, &args[1..]),
        Some("month") => cmd_month(&conn, &args[1..]),
        Some("serve") => serve::run(&conn, args.get(1).and_then(|p| p.parse().ok()).unwrap_or(7070)),
        _ => Err("usage: ledger add|list|month|serve".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("ledger: {msg}");
    ExitCode::FAILURE
}

fn cmd_add(conn: &rusqlite::Connection, a: &[String]) -> Result<(), String> {
    let [amount, category, note @ ..] = a else { return Err("usage: ledger add <amount> <category> [note]".into()) };
    let cents = db::parse_cents(amount)?;
    let id = db::add(conn, cents, category, &note.join(" ")).map_err(|e| e.to_string())?;
    println!("#{id}  {}  {category}", db::yuan(cents));
    Ok(())
}

fn cmd_list(conn: &rusqlite::Connection, a: &[String]) -> Result<(), String> {
    let n = a.first().and_then(|s| s.parse().ok()).unwrap_or(10);
    for e in db::latest(conn, n).map_err(|e| e.to_string())? {
        println!("#{:<4} {}  {:>10}  {} {}", e.id, e.day, db::yuan(e.cents), pad(&e.category, 8), e.note);
    }
    Ok(())
}

fn cmd_month(conn: &rusqlite::Connection, a: &[String]) -> Result<(), String> {
    let month = match a.first() {
        Some(m) => m.clone(),
        None => db::this_month(conn).map_err(|e| e.to_string())?,
    };
    let rows = db::month_totals(conn, &month).map_err(|e| e.to_string())?;
    let total: i64 = rows.iter().map(|r| r.1).sum();
    println!("{month}");
    for (cat, cents) in &rows {
        println!("  {} {:>10}", pad(cat, 8), db::yuan(*cents));
    }
    println!("  {} {:>10}", pad("合计", 8), db::yuan(total));
    Ok(())
}

/// Left-align to `w` terminal columns; CJK characters take two.
fn pad(s: &str, w: usize) -> String {
    let cols: usize = s.chars().map(|c| if (c as u32) >= 0x1100 { 2 } else { 1 }).sum();
    format!("{s}{}", " ".repeat(w.saturating_sub(cols)))
}
