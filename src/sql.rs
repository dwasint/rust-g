use jobs;
use mysql::consts::ColumnFlags;
use mysql::consts::ColumnType::*;
use mysql::{OptsBuilder, Params, Pool};
use serde_json::map::Map;
use serde_json::{json, Number};
use std::error::Error;
use std::sync::RwLock;
use std::time::Duration;

// ----------------------------------------------------------------------------
// Interface

// Defaults from `mysql` crate documentation.
const DEFAULT_PORT: u16 = 3306;
const DEFAULT_MIN_THREADS: usize = 10;
const DEFAULT_MAX_THREADS: usize = 100;

#[derive(Deserialize)]
struct ConnectOptions {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    pass: Option<String>,
    db_name: Option<String>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    min_threads: Option<usize>,
    max_threads: Option<usize>,
}

byond_fn! { sql_connect_pool(options) {
    let options = match serde_json::from_str::<ConnectOptions>(options) {
        Ok(options) => options,
        Err(e) => return Some(err_to_json(e)),
    };
    Some(match sql_connect(options) {
        Ok(o) => o.to_string(),
        Err(e) => err_to_json(e)
    })
} }

byond_fn! { sql_query_blocking(query, params) {
    Some(match do_query(query, params) {
        Ok(o) => o.to_string(),
        Err(e) => err_to_json(e)
    })
} }

byond_fn! { sql_query_async(query, params) {
    let query = query.to_owned();
    let params = params.to_owned();
    Some(jobs::start(move || {
        match do_query(&query, &params) {
            Ok(o) => o.to_string(),
            Err(e) => err_to_json(e)
        }
    }))
} }

// hopefully won't panic if queries are running
byond_fn! { sql_disconnect_pool() {
    Some(match POOL.write() {
        Ok(mut o) => {
            match o.take() {
                Some(_) => {
                    json!({
                        "status": "success"
                    }).to_string()
                },
                None => json!({
                    "status": "offline"
                }).to_string()
            }
        },
        Err(e) => err_to_json(e)
    })
} }

byond_fn! { sql_connected() {
    Some(match POOL.read() {
        Ok(o) => {
            match *o {
                Some(_) => json!({
                    "status": "online"
                }).to_string(),
                None => json!({
                    "status": "offline"
                }).to_string()
            }
        },
        Err(e) => err_to_json(e)
    })
} }

byond_fn! { sql_check_query(id) {
    Some(jobs::check(id))
} }

// ----------------------------------------------------------------------------
// Main connect and query implementation

lazy_static! {
    static ref POOL: RwLock<Option<Pool>> = RwLock::new(None);
}

fn sql_connect(options: ConnectOptions) -> Result<serde_json::Value, Box<dyn Error>> {
    let builder = OptsBuilder::new()
        .ip_or_hostname(options.host)
        .tcp_port(options.port.unwrap_or(DEFAULT_PORT))
        .user(options.user)
        .pass(options.pass)
        .db_name(options.db_name)
        .read_timeout(options.read_timeout)
        .write_timeout(options.write_timeout);
    let pool = Pool::new_manual(
        options.min_threads.unwrap_or(DEFAULT_MIN_THREADS),
        options.max_threads.unwrap_or(DEFAULT_MAX_THREADS),
        builder)?;
    let mut poolguard = POOL.write()?;
    *poolguard = Some(pool);
    Ok(json!({"status": "ok"}))
}

fn do_query(query: &str, params: &str) -> Result<serde_json::Value, Box<dyn Error>> {
    let affected;
    let mut rows: Vec<serde_json::Value> = Vec::new();

    {
        use mysql::prelude::Queryable;

        let mut conn = {
            let p = POOL.read()?;
            let pool = match &*p {
                Some(s) => s,
                None => return Ok(json!({"status": "offline"})),
            };
            pool.get_conn()?
        };

        let query_result = conn.exec_iter(query, params_from_json(params))?;
        affected = query_result.affected_rows();
        for row in query_result {
            let row = row?;
            let mut json_row: Vec<serde_json::Value> = Vec::new();
            for (i, col) in row.columns_ref().iter().enumerate() {
                let ctype = col.column_type();
                let value = row.as_ref(i).ok_or("length of row was smaller than column count")?;
                let converted = match value {
                    mysql::Value::Bytes(b) => match ctype {
                        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING => {
                            serde_json::Value::String(String::from_utf8_lossy(&b).into_owned())
                        }
                        MYSQL_TYPE_BLOB
                        | MYSQL_TYPE_LONG_BLOB
                        | MYSQL_TYPE_MEDIUM_BLOB
                        | MYSQL_TYPE_TINY_BLOB => {
                            if col.flags().contains(ColumnFlags::BINARY_FLAG) {
                                serde_json::Value::Array(
                                    b.into_iter()
                                        .map(|x| serde_json::Value::Number(Number::from(*x)))
                                        .collect(),
                                )
                            } else {
                                serde_json::Value::String(String::from_utf8_lossy(&b).into_owned())
                            }
                        }
                        _ => serde_json::Value::Null,
                    },
                    mysql::Value::Float(f) => {
                        serde_json::Value::Number(Number::from_f64(*f).unwrap_or(Number::from(0)))
                    }
                    mysql::Value::Int(i) => serde_json::Value::Number(Number::from(*i)),
                    mysql::Value::UInt(u) => serde_json::Value::Number(Number::from(*u)),
                    mysql::Value::Date(year, month, day, hour, minute, second, _ms) => {
                        serde_json::Value::String(format!(
                            "{}-{:02}-{:02} {:02}:{:02}:{:02}",
                            year, month, day, hour, minute, second
                        ))
                    }
                    _ => serde_json::Value::Null,
                };
                json_row.push(converted)
            }
            rows.push(serde_json::Value::Array(json_row));
        }
    };

    Ok(json! {{
        "status": "ok",
        "affected": affected,
        "rows": rows,
    }})
}

// ----------------------------------------------------------------------------
// Helpers

fn err_to_json<E: std::fmt::Display>(e: E) -> String {
    json!({
        "status": "err",
        "data": &e.to_string()
    })
    .to_string()
}

fn json_to_mysql(val: serde_json::Value) -> mysql::Value {
    match val {
        serde_json::Value::Bool(b) => mysql::Value::UInt(b as u64),
        serde_json::Value::Number(i) => {
            if let Some(v) = i.as_u64() {
                mysql::Value::UInt(v)
            } else if let Some(v) = i.as_i64() {
                mysql::Value::Int(v)
            } else if let Some(v) = i.as_f64() {
                mysql::Value::Float(v)
            } else {
                mysql::Value::NULL
            }
        }
        serde_json::Value::String(s) => mysql::Value::Bytes(s.into()),
        serde_json::Value::Array(a) => mysql::Value::Bytes(
            a.into_iter()
                .map(|x| {
                    if let serde_json::Value::Number(n) = x {
                        n.as_u64().unwrap_or(0) as u8
                    } else {
                        0
                    }
                })
                .collect(),
        ),
        _ => mysql::Value::NULL,
    }
}

fn array_to_params(params: Vec<serde_json::Value>) -> Params {
    if params.is_empty() {
        Params::Empty
    } else {
        Params::Positional(params.into_iter().map(json_to_mysql).collect())
    }
}

fn object_to_params(params: Map<std::string::String, serde_json::Value>) -> Params {
    if params.is_empty() {
        Params::Empty
    } else {
        Params::Named(
            params
                .into_iter()
                .map(|(key, val)| (key, json_to_mysql(val)))
                .collect(),
        )
    }
}

fn params_from_json(params: &str) -> Params {
    match serde_json::from_str(params) {
        Ok(serde_json::Value::Object(o)) => object_to_params(o),
        Ok(serde_json::Value::Array(a)) => array_to_params(a),
        _ => Params::Empty,
    }
}