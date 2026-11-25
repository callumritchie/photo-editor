//! Small date helpers (no time-zone database needed).

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn system_time_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// UTC civil date-time "YYYY-MM-DDTHH:MM:SS" from Unix milliseconds
/// (days-from-civil inverse, after H. Hinnant's public-domain algorithms).
pub fn utc_iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

/// Expand %Y %m %d %H %M %S in `template` from an ISO "YYYY-MM-DDTHH:MM:SS" string.
pub fn expand_template(template: &str, iso: &str) -> String {
    let part = |a: usize, b: usize| iso.get(a..b).unwrap_or("00");
    template
        .replace("%Y", part(0, 4))
        .replace("%m", part(5, 7))
        .replace("%d", part(8, 10))
        .replace("%H", part(11, 13))
        .replace("%M", part(14, 16))
        .replace("%S", part(17, 19))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(utc_iso_from_ms(0), "1970-01-01T00:00:00");
        assert_eq!(utc_iso_from_ms(951_782_400_000), "2000-02-29T00:00:00");
        assert_eq!(utc_iso_from_ms(1_700_000_000_000), "2023-11-14T22:13:20");
        assert_eq!(utc_iso_from_ms(-1000), "1969-12-31T23:59:59");
    }

    #[test]
    fn templates() {
        assert_eq!(
            expand_template("%Y/%Y-%m-%d", "2021-06-26T22:17:11"),
            "2021/2021-06-26"
        );
        assert_eq!(expand_template("%H%M%S", "2021-06-26T22:17:11"), "221711");
    }
}
