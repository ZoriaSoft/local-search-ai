//! Timed folder updates: a folder with a schedule is not watched live and is
//! not scanned at start-up; it is scanned once per time window instead (every
//! day or on chosen weekdays, between a start and an end time, in the
//! computer's local time). A run still going at the window's end is stopped;
//! what it indexed stays and the next window continues with the rest.

use chrono::{Datelike, Duration, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

pub const MINUTES_PER_DAY: u16 = 24 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSchedule {
    /// Weekdays the window opens on, 0 = Monday … 6 = Sunday; all seven for
    /// "every day". A window crossing midnight belongs to the day it opens.
    pub days: Vec<u8>,
    /// Window start and end, minutes after local midnight (0..1440). An end
    /// at or before the start means the window ends the next day.
    pub start: u16,
    pub end: u16,
}

impl FolderSchedule {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.days.is_empty() {
            return Err("Choose at least one day");
        }
        if self.days.iter().any(|&d| d > 6) {
            return Err("Days are 0 (Monday) to 6 (Sunday)");
        }
        if self.start >= MINUTES_PER_DAY || self.end >= MINUTES_PER_DAY {
            return Err("Times are minutes after midnight (0 to 1439)");
        }
        if self.start == self.end {
            return Err("The window needs different start and end times");
        }
        Ok(())
    }

    /// Sorted, without duplicates: the form stored and reported.
    pub fn normalized(mut self) -> Self {
        self.days.sort_unstable();
        self.days.dedup();
        self
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// `None` for a missing or unreadable value (the folder is then live).
    pub fn decode(value: Option<&str>) -> Option<Self> {
        let schedule: Self = serde_json::from_str(value?).ok()?;
        schedule.validate().ok()?;
        Some(schedule)
    }

    fn time(minutes: u16) -> NaiveTime {
        NaiveTime::from_hms_opt(u32::from(minutes / 60), u32::from(minutes % 60), 0).unwrap_or(NaiveTime::MIN)
    }

    fn window_opening(&self, date: chrono::NaiveDate) -> Option<(NaiveDateTime, NaiveDateTime)> {
        let weekday = date.weekday().num_days_from_monday() as u8;
        if !self.days.contains(&weekday) {
            return None;
        }
        let start = date.and_time(Self::time(self.start));
        let end_date = if self.end > self.start { date } else { date + Duration::days(1) };
        Some((start, end_date.and_time(Self::time(self.end))))
    }

    /// The window that is open at local time `now`, as (start, end).
    pub fn window_at(&self, now: NaiveDateTime) -> Option<(NaiveDateTime, NaiveDateTime)> {
        [now.date(), now.date() - Duration::days(1)]
            .into_iter()
            .filter_map(|date| self.window_opening(date))
            .find(|(start, end)| *start <= now && now < *end)
    }

    /// The next window start after local time `now` (within a week).
    pub fn next_start(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        (0..=7)
            .filter_map(|offset| self.window_opening(now.date() + Duration::days(offset)))
            .map(|(start, _)| start)
            .find(|start| *start > now)
    }
}

/// Unix seconds of a local wall-clock time (the earlier one in a DST fold;
/// a time skipped by DST is moved an hour forward).
pub fn local_to_unix(time: NaiveDateTime) -> i64 {
    use chrono::TimeZone as _;
    match chrono::Local.from_local_datetime(&time) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.timestamp(),
        chrono::LocalResult::None => chrono::Local
            .from_local_datetime(&(time + Duration::hours(1)))
            .earliest()
            .map_or_else(|| time.and_utc().timestamp(), |t| t.timestamp()),
    }
}

pub fn local_now() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn validation_and_round_trip() {
        let schedule = FolderSchedule { days: vec![4, 0, 0], start: 120, end: 300 }.normalized();
        assert_eq!(schedule.days, [0, 4]);
        assert!(schedule.validate().is_ok());
        assert_eq!(FolderSchedule::decode(Some(&schedule.encode())), Some(schedule));
        assert!(FolderSchedule { days: vec![], start: 1, end: 2 }.validate().is_err());
        assert!(FolderSchedule { days: vec![7], start: 1, end: 2 }.validate().is_err());
        assert!(FolderSchedule { days: vec![1], start: 1440, end: 2 }.validate().is_err());
        assert!(FolderSchedule { days: vec![1], start: 5, end: 5 }.validate().is_err());
        assert_eq!(FolderSchedule::decode(Some("bozuk")), None);
        assert_eq!(FolderSchedule::decode(None), None);
    }

    #[test]
    fn a_daytime_window_opens_only_on_its_days() {
        // Weekdays 12:30–13:30. 2026-11-25 is a Wednesday, 11-28 a Saturday.
        let schedule = FolderSchedule { days: vec![0, 1, 2, 3, 4], start: 12 * 60 + 30, end: 13 * 60 + 30 };
        assert_eq!(schedule.window_at(at("2026-11-25 12:45")), Some((at("2026-11-25 12:30"), at("2026-11-25 13:30"))));
        assert_eq!(schedule.window_at(at("2026-11-25 13:30")), None, "the end is exclusive");
        assert_eq!(schedule.window_at(at("2026-11-25 12:29")), None);
        assert_eq!(schedule.window_at(at("2026-11-28 12:45")), None, "Saturday is not selected");
        assert_eq!(schedule.next_start(at("2026-11-27 14:00")), Some(at("2026-11-30 12:30")), "Friday → Monday");
    }

    #[test]
    fn a_night_window_crosses_midnight_and_belongs_to_its_opening_day() {
        // Fridays 22:00–06:00: open Saturday 03:00, closed Sunday 03:00.
        let schedule = FolderSchedule { days: vec![4], start: 22 * 60, end: 6 * 60 };
        assert_eq!(schedule.window_at(at("2026-11-27 23:00")), Some((at("2026-11-27 22:00"), at("2026-11-28 06:00"))));
        assert_eq!(schedule.window_at(at("2026-11-28 03:00")), Some((at("2026-11-27 22:00"), at("2026-11-28 06:00"))));
        assert_eq!(schedule.window_at(at("2026-11-29 03:00")), None);
        assert_eq!(schedule.next_start(at("2026-11-28 03:00")), Some(at("2026-12-04 22:00")));
    }
}
