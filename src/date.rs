#![allow(dead_code)] // wired up in phase 3 (parser) and phase 4 (persistence)

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};

pub const DATE_FMT: &str = "%Y-%m-%d";
pub const DATETIME_FMT: &str = "%Y-%m-%d %H:%M";

pub fn now() -> DateTime<Local> {
    Local::now()
}

pub fn format_date(d: NaiveDate) -> String {
    d.format(DATE_FMT).to_string()
}

pub fn format_datetime(dt: NaiveDateTime) -> String {
    dt.format(DATETIME_FMT).to_string()
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, DATE_FMT).ok()
}

pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, DATETIME_FMT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_date() {
        let d = NaiveDate::from_ymd_opt(2026, 5, 12).unwrap();
        let s = format_date(d);
        assert_eq!(s, "2026-05-12");
        assert_eq!(parse_date(&s), Some(d));
    }

    #[test]
    fn round_trip_datetime() {
        let dt = NaiveDate::from_ymd_opt(2026, 5, 12)
            .unwrap()
            .and_hms_opt(15, 22, 0)
            .unwrap();
        let s = format_datetime(dt);
        assert_eq!(s, "2026-05-12 15:22");
        assert_eq!(parse_datetime(&s), Some(dt));
    }

    #[test]
    fn rejects_non_iso() {
        assert!(parse_date("12/05/2026").is_none());
        assert!(parse_datetime("2026-05-12T15:22").is_none());
    }
}
