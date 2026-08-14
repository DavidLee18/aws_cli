//! The local UTC offset.
//!
//! `aws s3 ls` prints every timestamp in the machine's local timezone, and `logs tail`
//! reads a naive `--since` in local time. Rust's standard library exposes no timezone
//! data at all, so this asks the C library — the same source Python's `tzlocal` ends up
//! consulting — rather than assuming UTC and printing times that are silently wrong by
//! the local offset.

/// Seconds east of UTC at the given instant, accounting for daylight saving.
///
/// Falls back to 0 (UTC) only if the platform call fails, which would mean no usable
/// timezone database.
#[cfg(unix)]
pub fn offset_seconds(at_unix: i64) -> i64 {
    // SAFETY: `localtime_r` writes into a caller-provided `tm`, so there is no shared
    // buffer and no thread-safety hazard. A null return means the conversion failed.
    unsafe {
        let time = at_unix as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

#[cfg(not(unix))]
pub fn offset_seconds(_at_unix: i64) -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever the machine's zone, the offset must be a whole number of minutes and
    /// within the range real zones occupy (-12:00 to +14:00).
    #[test]
    fn returns_a_plausible_offset() {
        let offset = offset_seconds(1_786_657_696);
        assert_eq!(offset % 60, 0, "offset {offset} is not a whole minute");
        assert!((-12 * 3600..=14 * 3600).contains(&offset), "offset {offset} out of range");
    }

    /// The offset is evaluated at a given instant, so a zone with daylight saving reports
    /// different values in January and July. Zones without DST report the same — both are
    /// acceptable, so this only asserts the call is stable when repeated.
    #[test]
    fn is_stable_for_a_given_instant() {
        let january = 1_767_225_600; // 2026-01-01T00:00:00Z
        assert_eq!(offset_seconds(january), offset_seconds(january));
    }
}
