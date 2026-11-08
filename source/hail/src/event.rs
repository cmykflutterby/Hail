use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EventKind {
    ServiceStarted,
    UsbInserted,
    UsbRemoved,
    PollingFailed,
}

impl EventKind {
    pub const ALL: [EventKind; 4] = [
        EventKind::ServiceStarted,
        EventKind::UsbInserted,
        EventKind::UsbRemoved,
        EventKind::PollingFailed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            EventKind::ServiceStarted => "service.started",
            EventKind::UsbInserted => "usb.inserted",
            EventKind::UsbRemoved => "usb.removed",
            EventKind::PollingFailed => "polling.failed",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Placeholders an action template may use for this event.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            EventKind::ServiceStarted => &[
                "event",
                "timestamp",
                "version",
                "config",
                "mode",
                "emulator",
            ],
            EventKind::UsbInserted => &[
                "event",
                "timestamp",
                "device",
                "label",
                "mount_point",
                "hail_cfg",
                "images",
            ],
            EventKind::UsbRemoved => &["event", "timestamp", "device", "label", "mount_point"],
            EventKind::PollingFailed => &["event", "timestamp", "error"],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    pub fields: BTreeMap<&'static str, String>,
}

impl Event {
    pub fn new(kind: EventKind, timestamp: String, values: &[(&'static str, String)]) -> Self {
        let mut fields = BTreeMap::new();
        fields.insert("event", kind.name().to_string());
        fields.insert("timestamp", timestamp);
        for (name, value) in values {
            fields.insert(*name, value.clone());
        }
        Self { kind, fields }
    }
}

/// Seconds since the Unix epoch (negative before 1970).
pub fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(error) => -(error.duration().as_secs() as i64),
    }
}

/// A calendar date and time of day (proleptic Gregorian calendar, no time zone).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CivilTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// ISO 8601 weekday: Monday = 1 ... Sunday = 7.
    pub weekday: u32,
    /// ISO 8601 week number (1-53) and the year that week belongs to.
    pub iso_week: u32,
    pub iso_week_year: i64,
}

/// Days since 1970-01-01 for a calendar date.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

impl CivilTime {
    /// Splits seconds since the Unix epoch (already shifted to the wanted time zone).
    pub fn from_unix(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let remainder = seconds.rem_euclid(86_400) as u32;
        let (year, month, day) = civil_from_days(days);
        // 1970-01-01 was a Thursday.
        let weekday = ((days + 3).rem_euclid(7) + 1) as u32;
        let thursday = days - i64::from(weekday) + 4;
        let iso_week_year = civil_from_days(thursday).0;
        let iso_week = ((thursday - days_from_civil(iso_week_year, 1, 1)) / 7 + 1) as u32;
        Self {
            year,
            month,
            day,
            hour: remainder / 3_600,
            minute: remainder % 3_600 / 60,
            second: remainder % 60,
            weekday,
            iso_week,
            iso_week_year,
        }
    }
}

/// Formats a time as RFC 3339 UTC with second precision.
pub fn utc_timestamp(time: SystemTime) -> String {
    let civil = CivilTime::from_unix(unix_seconds(time).max(0));
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second
    )
}
