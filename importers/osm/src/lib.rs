use std::fs::{self, File};
use std::path::Path;

use osmpbfreader::objects::Node;
use osmpbfreader::{OsmObj, OsmPbfReader};

use rusqlite::{Connection, DropBehavior, ToSql, NO_PARAMS};

struct Address {
    lat: f64,
    lon: f64,
    number: Option<String>,
    street: Option<String>,
    unit: Option<String>,
    city: Option<String>,
    district: Option<String>,
    region: Option<String>,
    postcode: Option<String>,
}

impl Address {
    fn new(node: Node) -> Address {
        let mut addr = Address {
            lat: node.lat(),
            lon: node.lon(),
            number: None,
            street: None,
            unit: None,
            city: None,
            district: None,
            region: None,
            postcode: None,
        };

        for (tag, value) in node.tags.iter() {
            match tag.as_str() {
                "addr:housenumber" => {
                    addr.number = Some(value.to_owned());
                }
                "addr:street" => {
                    addr.street = Some(value.to_owned());
                }
                "addr:unit" => {
                    addr.unit = Some(value.to_owned());
                }
                "addr:city" => {
                    addr.city = Some(value.to_owned());
                }
                "addr:district" => {
                    addr.district = Some(value.to_owned());
                }
                "addr:region" => {
                    addr.region = Some(value.to_owned());
                }
                "addr:postcode" => {
                    addr.postcode = Some(value.to_owned());
                }
                _ => {}
            }
        }
        addr
    }
}

pub struct DB {
    conn: Connection,
    buffer: Vec<Address>,
    db_buffer_size: usize,
}

impl DB {
    fn new(db_file: &str, db_buffer_size: usize, remove_db_data: bool) -> Result<DB, String> {
        let _ = fs::remove_file(db_file); // we ignore any potential error
        let conn = Connection::open(db_file)
            .map_err(|e| format!("failed to open SQLITE connection: {}", e))?;

        if remove_db_data {
            conn.execute("DROP TABLE IF EXISTS addresses", NO_PARAMS)
                .expect("failed to drop addresses");
            conn.execute("DROP TABLE IF EXISTS addresses_errors", NO_PARAMS)
                .expect("failed to drop errors");
        }
        conn.execute(
            r#"CREATE TABLE IF NOT EXISTS addresses(
                lat REAL NOT NULL,
                lon REAL NOT NULL,
                number TEXT,
                street TEXT NOT NULL,
                unit TEXT,
                city TEXT,
                district TEXT,
                region TEXT,
                postcode TEXT,
                PRIMARY KEY (lat, lon, number, street, city)
            )"#,
            NO_PARAMS,
        )
        .map_err(|e| format!("failed to create table: {}", e))?;
        conn.execute(
            r#"CREATE TABLE IF NOT EXISTS addresses_errors(
                lat REAL,
                lon REAL,
                number TEXT,
                street TEXT,
                unit TEXT,
                city TEXT,
                district TEXT,
                region TEXT,
                postcode TEXT,
                kind TEXT
            )"#,
            NO_PARAMS,
        )
        .map_err(|e| format!("failed to create error table: {}", e))?;
        Ok(DB {
            conn,
            buffer: Vec::with_capacity(db_buffer_size),
            db_buffer_size,
        })
    }

    fn flush_buffer(&mut self) {
        let mut tx = self.conn.transaction().expect("failed to open transaction");
        tx.set_drop_behavior(DropBehavior::Ignore);

        let mut errors = {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO addresses(
                    lat,
                    lon,
                    number,
                    street,
                    unit,
                    city,
                    district,
                    region,
                    postcode
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .expect("failed to prepare statement");

            self.buffer
                .drain(..)
                .filter_map(|obj| {
                    if let Err(e) = stmt.execute(&[
                        &obj.lat as &dyn ToSql,
                        &obj.lon,
                        &obj.number,
                        &obj.street,
                        &obj.unit,
                        &obj.city,
                        &obj.district,
                        &obj.region,
                        &obj.postcode,
                    ]) {
                        Some((obj, e.to_string()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        if !errors.is_empty() {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO addresses_errors(
                    lat,
                    lon,
                    number,
                    street,
                    unit,
                    city,
                    district,
                    region,
                    postcode,
                    kind
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )
                .expect("failed to prepare error statement");

            for (obj, err) in errors.drain(..) {
                stmt.execute(&[
                    &obj.lat as &dyn ToSql,
                    &obj.lon,
                    &obj.number,
                    &obj.street,
                    &obj.unit,
                    &obj.city,
                    &obj.district,
                    &obj.region,
                    &obj.postcode,
                    &err,
                ])
                .expect("failed to insert into errors");
            }
        }

        tx.commit().expect("commit failed");
    }

    fn insert(&mut self, addr: Address) {
        self.buffer.push(addr);
        if self.buffer.len() >= self.db_buffer_size {
            self.flush_buffer();
        }
    }

    pub fn get_nb_cities(&self) -> i64 {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM addresses GROUP BY city")
            .expect("failed to prepare");
        let mut iter = stmt
            .query_map(NO_PARAMS, |row| Ok(row.get(0)?))
            .expect("query_map failed");
        iter.next().expect("no count???").expect("failed")
    }

    pub fn get_nb_addresses(&self) -> i64 {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM addresses")
            .expect("failed to prepare");
        let mut iter = stmt
            .query_map(NO_PARAMS, |row| Ok(row.get(0)?))
            .expect("query_map failed");
        iter.next().expect("no count???").expect("failed")
    }

    pub fn get_nb_errors(&self) -> i64 {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM addresses_errors")
            .expect("failed to prepare");
        let mut iter = stmt
            .query_map(NO_PARAMS, |row| Ok(row.get(0)?))
            .expect("query_map failed");
        iter.next().expect("no count???").expect("failed")
    }

    pub fn get_nb_by_errors_kind(&self) -> Vec<(String, i64)> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM addresses_errors GROUP BY kind")
            .expect("failed to prepare");
        stmt.query_map(NO_PARAMS, |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query_map failed")
            .map(|x| x.expect("failed"))
            .collect()
    }
}

impl Drop for DB {
    fn drop(&mut self) {
        self.flush_buffer();
    }
}

pub fn import_addresses<P: AsRef<Path>>(
    db_file_name: &str,
    pbf_file: P,
    remove_db_data: bool,
) -> DB {
    let mut reader = OsmPbfReader::new(File::open(&pbf_file).expect(&format!(
        "Failed to open file `{}`",
        pbf_file.as_ref().display()
    )));
    let mut db = DB::new(db_file_name, 100, remove_db_data).expect("failed to create DB");
    for obj in reader.iter().filter_map(|o| match o {
        Ok(OsmObj::Node(o)) if o.tags.iter().any(|x| x.0.contains("addr:")) => Some(o),
        _ => None,
    }) {
        db.insert(Address::new(obj));
    }
    db.flush_buffer();
    db
}
