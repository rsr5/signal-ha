//! Lux-driven lighting helpers.
//!
//! Reusable primitives for rooms that adjust brightness and colour temperature
//! based on an ambient light sensor.  Ported from the bathroom automation's
//! `DaytimeLuxPolicy` / `calc_brightness_for_target_lux` / `calc_kelvin_from_lux`.

use crate::util::{clamp, linmap};

// ── Lux-targeting brightness ──────────────────────────────────────────

/// Policy for a proportional lux-targeting controller.
///
/// The controller computes the brightness percentage needed to "top up"
/// ambient light to a desired target lux.
#[derive(Debug, Clone)]
pub struct LuxTargetPolicy {
    /// Desired ambient lux when lights are on.
    pub target_lux: f64,
    /// Hysteresis band above target — lights turn off when
    /// `lux >= target_lux + deadband_lux`.
    pub deadband_lux: f64,
    /// Proportional gain: brightness-percent per lux of deficit.
    pub k_pct_per_lux: f64,
    /// Floor brightness when lights are needed at all (percent 0–100).
    pub min_brightness_pct: f64,
    /// Cap brightness (percent 0–100).
    pub max_brightness_pct: f64,
}

impl Default for LuxTargetPolicy {
    fn default() -> Self {
        Self {
            target_lux: 180.0,
            deadband_lux: 15.0,
            k_pct_per_lux: 0.35,
            min_brightness_pct: 5.0,
            max_brightness_pct: 100.0,
        }
    }
}

/// Compute the brightness percentage to reach `target_lux` given current
/// ambient `lux`.
///
/// Returns `None` when lights should be off (ambient already bright enough,
/// above target + deadband).  Otherwise returns a clamped percentage in
/// `[min_brightness_pct, max_brightness_pct]`.
pub fn brightness_for_target_lux(lux: f64, policy: &LuxTargetPolicy) -> Option<f64> {
    if lux >= policy.target_lux + policy.deadband_lux {
        return None;
    }
    let deficit = (policy.target_lux - lux).max(0.0);
    let raw = policy.min_brightness_pct + deficit * policy.k_pct_per_lux;
    Some(clamp(raw, policy.min_brightness_pct, policy.max_brightness_pct))
}

// ── Colour-temperature from lux ───────────────────────────────────────

/// Parameters for interpolating colour temperature from ambient lux.
///
/// At low lux → warm (k_min), at high lux → cool (k_max).  This mirrors
/// natural daylight: supplement with warm light when it's dim, cooler light
/// when it's already bright.
#[derive(Debug, Clone)]
pub struct CtFromLuxParams {
    /// Lux reading that maps to `k_min`.
    pub lux_low: f64,
    /// Lux reading that maps to `k_max`.
    pub lux_high: f64,
    /// Warmest colour temperature (Kelvin) — used at/below `lux_low`.
    pub k_min: f64,
    /// Coolest colour temperature (Kelvin) — used at/above `lux_high`.
    pub k_max: f64,
}

impl Default for CtFromLuxParams {
    fn default() -> Self {
        Self {
            lux_low: 10.0,
            lux_high: 250.0,
            k_min: 4000.0,
            k_max: 6000.0,
        }
    }
}

/// Compute colour temperature (Kelvin) from ambient lux.
///
/// Linearly interpolates between `k_min` at `lux_low` and `k_max` at
/// `lux_high`, clamped to that range.
pub fn ct_from_lux(lux: f64, params: &CtFromLuxParams) -> i32 {
    linmap(lux, params.lux_low, params.lux_high, params.k_min, params.k_max).round() as i32
}

// ── Time-of-day window ────────────────────────────────────────────────

/// A time-of-day window defined by `start` and `end` hours+minutes.
///
/// Supports both same-day windows (e.g. 07:00–19:30) and overnight windows
/// that cross midnight (e.g. 23:00–07:00).
#[derive(Debug, Clone)]
pub struct TimeWindow {
    /// Start hour (0–23).
    pub start_h: u32,
    /// Start minute (0–59).
    pub start_m: u32,
    /// End hour (0–23).
    pub end_h: u32,
    /// End minute (0–59).
    pub end_m: u32,
}

impl TimeWindow {
    pub fn new(start_h: u32, start_m: u32, end_h: u32, end_m: u32) -> Self {
        Self {
            start_h,
            start_m,
            end_h,
            end_m,
        }
    }

    /// Check whether `(hour, minute)` falls inside this window.
    ///
    /// Handles overnight wrap (start > end) correctly.
    pub fn contains(&self, hour: u32, minute: u32) -> bool {
        let now = hour * 60 + minute;
        let start = self.start_h * 60 + self.start_m;
        let end = self.end_h * 60 + self.end_m;

        if start <= end {
            // Same-day window: e.g. 07:00–19:30
            now >= start && now < end
        } else {
            // Overnight window: e.g. 23:00–07:00
            now >= start || now < end
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── brightness_for_target_lux ──────────────────────────────────

    fn default_policy() -> LuxTargetPolicy {
        LuxTargetPolicy::default()
    }

    #[test]
    fn bright_enough_returns_none() {
        // lux >= target + deadband → lights off
        let p = default_policy(); // target=180, deadband=15
        assert!(brightness_for_target_lux(195.0, &p).is_none());
        assert!(brightness_for_target_lux(200.0, &p).is_none());
    }

    #[test]
    fn just_below_deadband_returns_min() {
        let p = default_policy();
        // lux = target → deficit = 0 → raw = min_brightness = 5
        let b = brightness_for_target_lux(180.0, &p).unwrap();
        assert!((b - 5.0).abs() < 0.01);
    }

    #[test]
    fn dark_room_returns_high_brightness() {
        let p = default_policy();
        // lux = 0 → deficit = 180 → raw = 5 + 180*0.35 = 68
        let b = brightness_for_target_lux(0.0, &p).unwrap();
        assert!((b - 68.0).abs() < 0.01);
    }

    #[test]
    fn caps_at_max() {
        let p = LuxTargetPolicy {
            max_brightness_pct: 50.0,
            ..default_policy()
        };
        let b = brightness_for_target_lux(0.0, &p).unwrap();
        assert!((b - 50.0).abs() < 0.01);
    }

    #[test]
    fn never_below_min() {
        let p = LuxTargetPolicy {
            min_brightness_pct: 20.0,
            ..default_policy()
        };
        // lux = target → deficit = 0 → raw = 20 + 0 = 20
        let b = brightness_for_target_lux(180.0, &p).unwrap();
        assert!((b - 20.0).abs() < 0.01);
    }

    #[test]
    fn within_deadband_still_on() {
        let p = default_policy();
        // lux = 190 (target=180, deadband=15) → 190 < 195 → still on
        // deficit = max(0, 180-190) = 0 → raw = 5
        let b = brightness_for_target_lux(190.0, &p).unwrap();
        assert!((b - 5.0).abs() < 0.01);
    }

    // ── ct_from_lux ────────────────────────────────────────────────

    fn default_ct_params() -> CtFromLuxParams {
        CtFromLuxParams::default()
    }

    #[test]
    fn ct_at_low_lux() {
        assert_eq!(ct_from_lux(10.0, &default_ct_params()), 4000);
        assert_eq!(ct_from_lux(0.0, &default_ct_params()), 4000);
    }

    #[test]
    fn ct_at_high_lux() {
        assert_eq!(ct_from_lux(250.0, &default_ct_params()), 6000);
        assert_eq!(ct_from_lux(300.0, &default_ct_params()), 6000);
    }

    #[test]
    fn ct_mid_lux() {
        // 130 is midpoint of 10..250 → should be midpoint of 4000..6000 = 5000
        assert_eq!(ct_from_lux(130.0, &default_ct_params()), 5000);
    }

    #[test]
    fn ct_quarter_lux() {
        // 70 = 10 + 0.25*(250-10) = 70 → k = 4000 + 0.25*2000 = 4500
        assert_eq!(ct_from_lux(70.0, &default_ct_params()), 4500);
    }

    #[test]
    fn ct_matches_bathroom_python() {
        // Cross-check with the Python calc_kelvin_from_lux behaviour
        let p = CtFromLuxParams {
            lux_low: 10.0,
            lux_high: 250.0,
            k_min: 4000.0,
            k_max: 6000.0,
        };
        assert_eq!(ct_from_lux(5.0, &p), 4000);   // below range → k_min
        assert_eq!(ct_from_lux(10.0, &p), 4000);   // at low
        assert_eq!(ct_from_lux(250.0, &p), 6000);  // at high
        assert_eq!(ct_from_lux(500.0, &p), 6000);  // above range → k_max
    }

    // ── TimeWindow ─────────────────────────────────────────────────

    #[test]
    fn same_day_window() {
        let w = TimeWindow::new(7, 0, 19, 30);
        assert!(w.contains(7, 0));
        assert!(w.contains(12, 0));
        assert!(w.contains(19, 29));
        assert!(!w.contains(19, 30));
        assert!(!w.contains(6, 59));
        assert!(!w.contains(20, 0));
    }

    #[test]
    fn overnight_window() {
        let w = TimeWindow::new(23, 0, 7, 0);
        assert!(w.contains(23, 0));
        assert!(w.contains(0, 0));
        assert!(w.contains(3, 30));
        assert!(w.contains(6, 59));
        assert!(!w.contains(7, 0));
        assert!(!w.contains(12, 0));
        assert!(!w.contains(22, 59));
    }

    #[test]
    fn overnight_window_nightlight() {
        // 23:00–07:00 late nightlight window
        let w = TimeWindow::new(23, 0, 7, 0);
        assert!(w.contains(23, 0));
        assert!(w.contains(2, 0));
        assert!(!w.contains(22, 0));
        assert!(!w.contains(7, 0));
    }

    #[test]
    fn zero_width_window() {
        // Same start and end → same-day path, nothing matches
        let w = TimeWindow::new(12, 0, 12, 0);
        assert!(!w.contains(12, 0));
        assert!(!w.contains(11, 59));
    }

    #[test]
    fn full_day_window() {
        // 00:00–00:00 wraps → overnight path → everything matches
        // because start == end → start <= end path (same-day), nothing matches
        // This is the edge case — use 0:00–23:59 for "always"
        let w = TimeWindow::new(0, 0, 23, 59);
        assert!(w.contains(0, 0));
        assert!(w.contains(12, 0));
        assert!(w.contains(23, 58));
        assert!(!w.contains(23, 59));
    }
}
