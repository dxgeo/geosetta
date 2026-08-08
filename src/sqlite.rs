//! A minimal, read-only SQLite file-format reader, implemented from the format
//! spec rather than a crate (keeping the project dependency-free). Enough to
//! enumerate tables and scan their rows — the basis for reading GeoPackage.
//!
//! It reads the 100-byte database header, walks table b-trees (interior + leaf
//! pages, following overflow chains), and decodes the record format
//! (serial-type varints → typed values). Multi-byte integers in the header and
//! b-tree structures are big-endian.

use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::Convert(format!("sqlite: {msg}")))
}

const HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// A decoded cell value (SQLite storage classes).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A table from `sqlite_master`, with its column names parsed from the DDL.
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    root_page: u32,
    /// Index of the `INTEGER PRIMARY KEY` column (a rowid alias), if any: its
    /// stored value is NULL and the real value is the rowid.
    rowid_alias: Option<usize>,
}

impl Table {
    /// The index of the `INTEGER PRIMARY KEY` (rowid-alias) column, if any.
    pub fn rowid_alias(&self) -> Option<usize> {
        self.rowid_alias
    }
}

pub struct Database<'a> {
    data: &'a [u8],
    page_size: usize,
    usable_size: usize,
}

impl<'a> Database<'a> {
    pub fn open(data: &'a [u8]) -> Result<Database<'a>> {
        if data.len() < 100 || &data[0..16] != HEADER_MAGIC {
            return err("not a sqlite database (bad magic)");
        }
        let page_size = match u16::from_be_bytes([data[16], data[17]]) {
            1 => 65536,
            n => n as usize,
        };
        if page_size < 512 || data.len() < page_size {
            return err("invalid page size");
        }
        let reserved = data[20] as usize;
        let encoding = u32::from_be_bytes([data[56], data[57], data[58], data[59]]);
        if encoding != 1 {
            return err("only UTF-8 databases are supported");
        }
        Ok(Database {
            data,
            page_size,
            usable_size: page_size - reserved,
        })
    }

    /// The page-1-based byte slice for page `n` (1-indexed).
    fn page(&self, n: u32) -> Result<&'a [u8]> {
        let n = n as usize;
        if n == 0 {
            return err("page 0 is invalid");
        }
        let start = (n - 1) * self.page_size;
        self.data
            .get(start..start + self.page_size)
            .ok_or_else(|| Error::Convert("sqlite: page out of range".into()))
    }

    /// All ordinary tables (`type = 'table'`) with a real root page.
    pub fn tables(&self) -> Result<Vec<Table>> {
        // sqlite_master is a table b-tree rooted at page 1. Its columns are
        // (type, name, tbl_name, rootpage, sql).
        let master = Table {
            name: "sqlite_master".into(),
            columns: vec!["type".into(), "name".into(), "tbl_name".into(), "rootpage".into(), "sql".into()],
            root_page: 1,
            rowid_alias: None,
        };
        let mut tables = Vec::new();
        for row in self.read_rows(&master)? {
            let kind = as_text(&row, 0);
            let root = as_int(&row, 3);
            let sql = as_text(&row, 4);
            if kind.as_deref() == Some("table")
                && let (Some(name), Some(root), Some(sql)) = (as_text(&row, 1), root, sql)
                && root > 0
            {
                let (columns, rowid_alias) = parse_columns(&sql);
                tables.push(Table {
                    name,
                    columns,
                    root_page: root as u32,
                    rowid_alias,
                });
            }
        }
        Ok(tables)
    }

    /// Every row of `table`, values aligned to `table.columns`.
    pub fn read_rows(&self, table: &Table) -> Result<Vec<Vec<Value>>> {
        let mut cells = Vec::new();
        self.walk_table(table.root_page, &mut cells)?;
        let ncols = table.columns.len();

        let mut rows = Vec::with_capacity(cells.len());
        for (rowid, payload) in cells {
            let mut values = decode_record(&payload)?;
            values.resize(ncols, Value::Null); // pad/truncate to the column count
            values.truncate(ncols);
            if let Some(i) = table.rowid_alias
                && matches!(values.get(i), Some(Value::Null))
            {
                values[i] = Value::Int(rowid);
            }
            rows.push(values);
        }
        Ok(rows)
    }

    /// Depth-first walk of a table b-tree, collecting `(rowid, payload)`.
    fn walk_table(&self, page_no: u32, out: &mut Vec<(i64, Vec<u8>)>) -> Result<()> {
        let page = self.page(page_no)?;
        let base = if page_no == 1 { 100 } else { 0 };
        let page_type = page[base];
        let num_cells = u16::from_be_bytes([page[base + 3], page[base + 4]]) as usize;

        match page_type {
            0x0d => {
                // Leaf table page: 8-byte header, then cell pointers.
                let ptr_array = base + 8;
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([page[ptr_array + i * 2], page[ptr_array + i * 2 + 1]])
                        as usize;
                    let (rowid, payload) = self.leaf_cell(page, off)?;
                    out.push((rowid, payload));
                }
            }
            0x05 => {
                // Interior table page: 12-byte header (right pointer at base+8).
                let ptr_array = base + 12;
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([page[ptr_array + i * 2], page[ptr_array + i * 2 + 1]])
                        as usize;
                    let child = u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]);
                    self.walk_table(child, out)?;
                }
                let right = u32::from_be_bytes([page[base + 8], page[base + 9], page[base + 10], page[base + 11]]);
                self.walk_table(right, out)?;
            }
            other => return err(&format!("unexpected b-tree page type {other:#x}")),
        }
        Ok(())
    }

    /// Decode a leaf table cell at `off`, assembling any overflow payload.
    fn leaf_cell(&self, page: &[u8], off: usize) -> Result<(i64, Vec<u8>)> {
        let (payload_len, n1) = varint(page, off)?;
        let (rowid, n2) = varint(page, off + n1)?;
        let payload_len = payload_len as usize;
        let content = off + n1 + n2;

        // How much of the payload lives on this page.
        let u = self.usable_size;
        let max_local = u - 35;
        let local = if payload_len <= max_local {
            payload_len
        } else {
            let min_local = ((u - 12) * 32 / 255) - 23;
            let k = min_local + (payload_len - min_local) % (u - 4);
            if k <= max_local { k } else { min_local }
        };

        let mut payload = page
            .get(content..content + local)
            .ok_or_else(|| Error::Convert("sqlite: cell payload out of range".into()))?
            .to_vec();

        if payload_len > local {
            let mut next = u32::from_be_bytes([
                page[content + local],
                page[content + local + 1],
                page[content + local + 2],
                page[content + local + 3],
            ]);
            while next != 0 && payload.len() < payload_len {
                let ov = self.page(next)?;
                next = u32::from_be_bytes([ov[0], ov[1], ov[2], ov[3]]);
                let take = (payload_len - payload.len()).min(u - 4);
                payload.extend_from_slice(&ov[4..4 + take]);
            }
        }
        if payload.len() < payload_len {
            return err("truncated overflow payload");
        }
        Ok((rowid, payload))
    }
}

// --- record decoding -------------------------------------------------------

/// Decode a record payload into its column values.
fn decode_record(payload: &[u8]) -> Result<Vec<Value>> {
    let (header_len, mut hp) = varint(payload, 0)?;
    let header_len = header_len as usize;
    let mut bp = header_len;
    let mut values = Vec::new();
    while hp < header_len {
        let (serial, adv) = varint(payload, hp)?;
        hp += adv;
        let (value, size) = decode_value(serial, &payload[bp.min(payload.len())..])?;
        bp += size;
        values.push(value);
    }
    Ok(values)
}

fn decode_value(serial: i64, data: &[u8]) -> Result<(Value, usize)> {
    let need = |n: usize| -> Result<&[u8]> {
        data.get(..n)
            .ok_or_else(|| Error::Convert("sqlite: truncated value".into()))
    };
    Ok(match serial {
        0 => (Value::Null, 0),
        1 => (Value::Int(signed_be(need(1)?)), 1),
        2 => (Value::Int(signed_be(need(2)?)), 2),
        3 => (Value::Int(signed_be(need(3)?)), 3),
        4 => (Value::Int(signed_be(need(4)?)), 4),
        5 => (Value::Int(signed_be(need(6)?)), 6),
        6 => (Value::Int(signed_be(need(8)?)), 8),
        7 => {
            let b = need(8)?;
            (Value::Real(f64::from_be_bytes(b.try_into().unwrap())), 8)
        }
        8 => (Value::Int(0), 0),
        9 => (Value::Int(1), 0),
        n if n >= 12 && n % 2 == 0 => {
            let len = (n as usize - 12) / 2;
            (Value::Blob(need(len)?.to_vec()), len)
        }
        n if n >= 13 => {
            let len = (n as usize - 13) / 2;
            let s = std::str::from_utf8(need(len)?)
                .map_err(|_| Error::Convert("sqlite: invalid utf-8 text".into()))?;
            (Value::Text(s.to_string()), len)
        }
        other => return err(&format!("reserved serial type {other}")),
    })
}

/// Sign-extend a big-endian signed integer of 1..8 bytes.
fn signed_be(bytes: &[u8]) -> i64 {
    let mut v: u64 = if bytes[0] & 0x80 != 0 { u64::MAX } else { 0 };
    for &b in bytes {
        v = (v << 8) | b as u64;
    }
    v as i64
}

/// A SQLite varint: big-endian, up to 9 bytes, high bit as continuation.
fn varint(data: &[u8], pos: usize) -> Result<(i64, usize)> {
    let mut result: u64 = 0;
    for i in 0..8 {
        let b = *data
            .get(pos + i)
            .ok_or_else(|| Error::Convert("sqlite: truncated varint".into()))?;
        result = (result << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return Ok((result as i64, i + 1));
        }
    }
    let b = *data
        .get(pos + 8)
        .ok_or_else(|| Error::Convert("sqlite: truncated varint".into()))?;
    result = (result << 8) | b as u64;
    Ok((result as i64, 9))
}

// --- CREATE TABLE column parsing -------------------------------------------

/// Extract column names (in order) and the rowid-alias column index from a
/// `CREATE TABLE` statement.
fn parse_columns(sql: &str) -> (Vec<String>, Option<usize>) {
    let Some(open) = sql.find('(') else {
        return (Vec::new(), None);
    };
    let Some(close) = sql.rfind(')') else {
        return (Vec::new(), None);
    };
    let body = &sql[open + 1..close];

    let mut names = Vec::new();
    let mut rowid_alias = None;
    for part in split_top_level(body) {
        let part = part.trim();
        if part.is_empty() || is_table_constraint(part) {
            continue;
        }
        let Some(name) = first_identifier(part) else {
            continue;
        };
        let upper = part.to_ascii_uppercase();
        if upper.contains("INTEGER") && upper.contains("PRIMARY KEY") {
            rowid_alias = Some(names.len());
        }
        names.push(name);
    }
    (names, rowid_alias)
}

/// Split on top-level commas, respecting parentheses and quotes.
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '[' => {
                    quote = Some(']');
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    parts.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            },
        }
    }
    parts.push(cur);
    parts
}

fn is_table_constraint(part: &str) -> bool {
    let word = part
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        word.as_str(),
        "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
    )
}

/// The column name at the start of a column definition, stripping any quoting.
fn first_identifier(part: &str) -> Option<String> {
    let s = part.trim_start();
    let mut chars = s.chars();
    let first = chars.next()?;
    let (open, close) = match first {
        '"' => ('"', '"'),
        '`' => ('`', '`'),
        '[' => ('[', ']'),
        _ => {
            // Unquoted: up to the next whitespace or '('.
            let name: String = s
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '(')
                .collect();
            return (!name.is_empty()).then_some(name);
        }
    };
    let _ = open;
    let mut name = String::new();
    for c in chars {
        if c == close {
            break;
        }
        name.push(c);
    }
    (!name.is_empty()).then_some(name)
}

fn as_text(row: &[Value], i: usize) -> Option<String> {
    match row.get(i) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn as_int(row: &[Value], i: usize) -> Option<i64> {
    match row.get(i) {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_values() {
        assert_eq!(varint(&[0x00], 0).unwrap(), (0, 1));
        assert_eq!(varint(&[0x7f], 0).unwrap(), (127, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (128, 2));
        assert_eq!(varint(&[0x82, 0x00], 0).unwrap(), (256, 2));
    }

    #[test]
    fn parses_column_ddl() {
        let sql = r#"CREATE TABLE "places" ( "fid" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, "geom" GEOMETRY, "name" TEXT, "pop" MEDIUMINT, "score" DOUBLE)"#;
        let (cols, alias) = parse_columns(sql);
        assert_eq!(cols, vec!["fid", "geom", "name", "pop", "score"]);
        assert_eq!(alias, Some(0));
    }

    #[test]
    fn reads_a_real_geopackage() {
        // A DuckDB-written GPKG: layer "places" with fid/geom/name/pop/score.
        let bytes = include_bytes!("../tests/fixtures/points.gpkg");
        let db = Database::open(bytes).unwrap();
        let tables = db.tables().unwrap();

        // Required GPKG metadata tables plus the feature table are present.
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"gpkg_contents"));
        assert!(names.contains(&"places"));

        let places = tables.iter().find(|t| t.name == "places").unwrap();
        assert_eq!(places.columns, vec!["fid", "geom", "name", "pop", "score"]);
        let rows = db.read_rows(places).unwrap();
        assert_eq!(rows.len(), 2);

        // fid is the rowid alias -> filled from the rowid (1, 2).
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[0][2], Value::Text("Alpha".into()));
        assert_eq!(rows[0][3], Value::Int(100));
        assert_eq!(rows[0][4], Value::Real(1.5));
        assert!(matches!(rows[0][1], Value::Blob(_))); // GPKG geometry blob
        assert_eq!(rows[1][2], Value::Text("Beta".into()));
        assert_eq!(rows[1][3], Value::Int(200));
    }
}
