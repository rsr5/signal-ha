//! Sun-aware timer primitives.
//!
//! Uses the `sunrise` crate for solar calculations and `tokio::time` for
//! the actual timers. No custom scheduler — the OS timer wheel does the work.

use chrono::{Datelike, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use sunrise::{Coordinates, SolarDay, SolarEvent};
use tokio_stream::Stream;
use tracing::debug;

/// Sun-aware scheduler for a fixed geographic location.
///
/// All times are in the system's local timezone.
#[derive(Debug, Clone)]
pub struct Scheduler {
    coords: Coordinates,
}

impl Scheduler {
    /// Create a new scheduler for the given latitude and longitude.
    ///
    /// # Example
    /// ```
    /// use signal_ha::Scheduler;
    /// let sched = Scheduler::new(48.86, 2.35); // Paris
    /// ```
    pub fn new(latitude: f64, longitude: f64) -> Self {
        let coords =
            Coordinates::new(latitude, longitude).expect("Invalid coordinates");
        Self { coords }
    }

    /// Calculate sunrise for a given date as a UTC DateTime.
    fn sunrise_utc(
        &self,
        year: i32,
        month: u32,
        day: u32,
    ) -> chrono::DateTime<Utc> {
        let date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        let solar_day = SolarDay::new(self.coords, date);
        let ts = solar_day
            .event_time(SolarEvent::Sunrise)
            .expect("no sunrise for this date");
        Utc.timestamp_opt(ts.timestamp(), 0).unwrap()
    }

    /// Calculate sunset for a given date as a UTC DateTime.
    fn sunset_utc(
        &self,
        year: i32,
        month: u32,
        day: u32,
    ) -> chrono::DateTime<Utc> {
        let date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        let solar_day = SolarDay::new(self.coords, date);
        let ts = solar_day
            .event_time(SolarEvent::Sunset)
            .expect("no sunset for this date");
        Utc.timestamp_opt(ts.timestamp(), 0).unwrap()
    }

    /// Next sunrise in local time.
    pub fn next_sunrise(&self) -> chrono::DateTime<Local> {
        let now = Local::now();
        let today_sunrise = self
            .sunrise_utc(now.year(), now.month(), now.day())
            .with_timezone(&Local);
        if today_sunrise > now {
            today_sunrise
        } else {
            let tomorrow = now + chrono::Duration::days(1);
            self.sunrise_utc(tomorrow.year(), tomorrow.month(), tomorrow.day())
                .with_timezone(&Local)
        }
    }

    /// Next sunset in local time.
    pub fn next_sunset(&self) -> chrono::DateTime<Local> {
        let now = Local::now();
        let today_sunset = self
            .sunset_utc(now.year(), now.month(), now.day())
            .with_timezone(&Local);
        if today_sunset > now {
            today_sunset
        } else {
            let tomorrow = now + chrono::Duration::days(1);
            self.sunset_utc(tomorrow.year(), tomorrow.month(), tomorrow.day())
                .with_timezone(&Local)
        }
    }

    /// Whether the sun is currently above the horizon.
    pub fn is_sun_up(&self) -> bool {
        let now = Local::now();
        let sunrise = self
            .sunrise_utc(now.year(), now.month(), now.day())
            .with_timezone(&Local);
        let sunset = self
            .sunset_utc(now.year(), now.month(), now.day())
            .with_timezone(&Local);
        now >= sunrise && now < sunset
    }

    /// Returns a stream that fires at sunrise + offset each day.
    ///
    /// The first tick may be tomorrow if today's sunrise has already passed.
    pub fn at_sunrise(
        &self,
        offset: chrono::Duration,
    ) -> impl Stream<Item = chrono::DateTime<Local>> + Send + 'static {
        let sched = self.clone();
        futures::stream::unfold((), move |()| {
            let sched = sched.clone();
            async move {
                let target = sched.next_sunrise() + offset;
                let now = Local::now();
                if target > now {
                    let wait = (target - now).to_std().unwrap_or_default();
                    debug!(?target, ?wait, "Sleeping until sunrise");
                    tokio::time::sleep(wait).await;
                }
                Some((target, ()))
            }
        })
    }

    /// Returns a stream that fires at sunset + offset each day.
    pub fn at_sunset(
        &self,
        offset: chrono::Duration,
    ) -> impl Stream<Item = chrono::DateTime<Local>> + Send + 'static {
        let sched = self.clone();
        futures::stream::unfold((), move |()| {
            let sched = sched.clone();
            async move {
                let target = sched.next_sunset() + offset;
                let now = Local::now();
                if target > now {
                    let wait = (target - now).to_std().unwrap_or_default();
                    debug!(?target, ?wait, "Sleeping until sunset");
                    tokio::time::sleep(wait).await;
                }
                Some((target, ()))
            }
        })
    }

    /// Returns a stream that fires daily at a fixed local time.
    pub fn daily(
        &self,
        time: NaiveTime,
    ) -> impl Stream<Item = chrono::DateTime<Local>> + Send + 'static {
        futures::stream::unfold(time, move |t| async move {
            let now = Local::now();
            let today = now
                .date_naive()
                .and_time(t)
                .and_local_timezone(Local)
                .single();
            let target = match today {
                Some(dt) if dt > now => dt,
                _ => {
                    let tomorrow = (now + chrono::Duration::days(1)).date_naive();
                    tomorrow.and_time(t).and_local_timezone(Local).unwrap()
                }
            };
            let wait = (target - now).to_std().unwrap_or_default();
            debug!(?target, ?wait, "Sleeping until daily timer");
            tokio::time::sleep(wait).await;
            Some((target, t))
        })
    }

    /// One-shot future that resolves after a duration.
    pub fn after(
        &self,
        duration: std::time::Duration,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        tokio::time::sleep(duration)
    }
}
