
use chrono::{Utc, TimeZone};

pub fn mongodb_date_time_format(dt: &mongodb::bson::DateTime) -> String {
    let chrono_dt = Utc.timestamp_millis_opt(dt.timestamp_millis()).unwrap();
    chrono_dt.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}