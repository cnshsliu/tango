//! A std-only HTTP server: GET /entries and GET /month return JSON. One request at a time.
use crate::db;
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

pub fn run(conn: &Connection, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind {port}: {e}"))?;
    println!("ledger serving on http://127.0.0.1:{port}  (GET /entries, GET /month)");
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        let mut line = String::new();
        if BufReader::new(&s).read_line(&mut line).is_err() {
            continue;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let (status, body) = route(conn, &path);
        println!("GET {path} → {status}");
        let _ = write!(
            s,
            "HTTP/1.1 {status}\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    Ok(())
}

fn route(conn: &Connection, path: &str) -> (&'static str, String) {
    let r = match path {
        "/entries" => db::latest(conn, 50).map(|es| {
            let items: Vec<String> = es
                .iter()
                .map(|e| format!(r#"{{"id":{},"day":"{}","cents":{},"category":{},"note":{}}}"#, e.id, e.day, e.cents, q(&e.category), q(&e.note)))
                .collect();
            format!("[{}]", items.join(","))
        }),
        "/month" => db::this_month(conn).and_then(|m| db::month_totals(conn, &m)).map(|rows| {
            let items: Vec<String> = rows.iter().map(|(c, v)| format!("{}:{v}", q(c))).collect();
            format!("{{{}}}", items.join(","))
        }),
        _ => return ("404 Not Found", r#"{"error":"not found"}"#.into()),
    };
    match r {
        Ok(body) => ("200 OK", body),
        Err(e) => ("500 Internal Server Error", format!(r#"{{"error":{}}}"#, q(&e.to_string()))),
    }
}

/// JSON string literal.
fn q(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
