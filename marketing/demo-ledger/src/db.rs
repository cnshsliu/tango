//! SQLite storage. Amounts are integer cents so sums never drift.
use rusqlite::{params, Connection, Result};

pub struct Entry {
    pub id: i64,
    pub day: String,
    pub cents: i64,
    pub category: String,
    pub note: String,
}

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS entries (
           id       INTEGER PRIMARY KEY,
           day      TEXT NOT NULL DEFAULT (date('now','localtime')),
           cents    INTEGER NOT NULL,
           category TEXT NOT NULL,
           note     TEXT NOT NULL DEFAULT ''
         );",
    )?;
    Ok(conn)
}

pub fn add(conn: &Connection, cents: i64, category: &str, note: &str) -> Result<i64> {
    conn.execute("INSERT INTO entries (cents, category, note) VALUES (?1, ?2, ?3)", params![cents, category, note])?;
    Ok(conn.last_insert_rowid())
}

pub fn latest(conn: &Connection, n: i64) -> Result<Vec<Entry>> {
    let mut st = conn.prepare("SELECT id, day, cents, category, note FROM entries ORDER BY id DESC LIMIT ?1")?;
    let rows = st.query_map([n], |r| {
        Ok(Entry { id: r.get(0)?, day: r.get(1)?, cents: r.get(2)?, category: r.get(3)?, note: r.get(4)? })
    })?;
    rows.collect()
}

pub fn this_month(conn: &Connection) -> Result<String> {
    conn.query_row("SELECT strftime('%Y-%m','now','localtime')", [], |r| r.get(0))
}

/// (category, cents) for one YYYY-MM, biggest first.
pub fn month_totals(conn: &Connection, month: &str) -> Result<Vec<(String, i64)>> {
    let mut st = conn.prepare(
        "SELECT category, SUM(cents) FROM entries WHERE substr(day,1,7) = ?1 GROUP BY category ORDER BY 2 DESC",
    )?;
    let rows = st.query_map([month], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// "38.5" / "38.50" / "38" → 3850. Rejects negatives and more than two decimals.
pub fn parse_cents(s: &str) -> std::result::Result<i64, String> {
    let bad = || format!("not an amount: {s}");
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() || frac.len() > 2 || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let int: i64 = int.parse().map_err(|_| bad())?;
    let frac: i64 = format!("{frac:0<2}").parse().map_err(|_| bad())?;
    Ok(int * 100 + frac)
}

pub fn yuan(cents: i64) -> String {
    format!("¥{}.{:02}", cents / 100, cents % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cents_round_trip() {
        assert_eq!(parse_cents("38.5"), Ok(3850));
        assert_eq!(parse_cents("12"), Ok(1200));
        assert_eq!(parse_cents("0.05"), Ok(5));
        assert!(parse_cents("1.234").is_err());
        assert!(parse_cents("-3").is_err());
        assert_eq!(yuan(3850), "¥38.50");
    }

    #[test]
    fn month_sums_by_category() {
        let c = open(":memory:").unwrap();
        add(&c, 3850, "餐饮", "午饭").unwrap();
        add(&c, 1200, "交通", "地铁").unwrap();
        add(&c, 2000, "餐饮", "咖啡").unwrap();
        let m = this_month(&c).unwrap();
        assert_eq!(month_totals(&c, &m).unwrap(), vec![("餐饮".into(), 5850), ("交通".into(), 1200)]);
    }
}
