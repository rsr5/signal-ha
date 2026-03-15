//! Shared utility functions for lighting.
//!
//! Pure functions with no HA/async dependencies.
//! Direct port of `appdaemon_lighting.utils`.

/// Clamp value to `[lo, hi]`.
pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}

/// Clamp and round to integer.
pub fn clamp_int(x: f64, lo: i32, hi: i32) -> i32 {
    (x.round() as i32).max(lo).min(hi)
}

/// Parse string to float, returning `default` on failure.
///
/// Handles HA "unknown", "unavailable", empty, and malformed values.
pub fn safe_float(s: Option<&str>, default: f64) -> f64 {
    match s {
        None => default,
        Some(raw) => {
            let trimmed = raw.trim();
            let lower = trimmed.to_lowercase();
            if lower.is_empty()
                || lower == "unknown"
                || lower == "unavailable"
                || lower == "none"
            {
                return default;
            }
            trimmed.parse::<f64>().unwrap_or(default)
        }
    }
}

/// Convert 0–100% to HA brightness 0–255.
pub fn pct_to_ha_brightness(pct: f64) -> i32 {
    (clamp(pct, 0.0, 100.0) * 255.0 / 100.0).round() as i32
}

/// Convert HA brightness 0–255 to 0–100%.
pub fn ha_brightness_to_pct(brightness: i32) -> f64 {
    clamp(brightness as f64, 0.0, 255.0) * 100.0 / 255.0
}

/// Convert Kelvin to mired (micro reciprocal degrees).
pub fn kelvin_to_mired(kelvin: i32) -> i32 {
    if kelvin <= 0 {
        return 0;
    }
    (1_000_000.0 / kelvin as f64).round() as i32
}

/// Linear interpolation with clamping.
///
/// Maps `x` from range `[x0, x1]` to `[y0, y1]`, clamping to output range.
/// If `x0 == x1`, returns `y1` if `x >= x1`, else `y0`.
pub fn linmap(x: f64, x0: f64, x1: f64, y0: f64, y1: f64) -> f64 {
    if (x1 - x0).abs() < f64::EPSILON {
        return if x >= x1 { y1 } else { y0 };
    }
    let t = clamp((x - x0) / (x1 - x0), 0.0, 1.0);
    y0 + t * (y1 - y0)
}

/// Cubic smoothstep: maps `[0, 1] → [0, 1]` with zero derivative at
/// endpoints. Formula: `3p² − 2p³`.
pub fn smoothstep(p: f64) -> f64 {
    let p = clamp(p, 0.0, 1.0);
    3.0 * p * p - 2.0 * p * p * p
}

/// Linear interpolation between `a` and `b`.
///
/// `t` is *not* clamped — it can extrapolate.
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Robustly interpret HA values as bool.
///
/// Accepts: "true", "on", "yes", "y", "1" → true; everything else → false.
pub fn as_bool(s: Option<&str>) -> bool {
    match s {
        None => false,
        Some(raw) => {
            let lower = raw.trim().to_lowercase();
            matches!(lower.as_str(), "true" | "on" | "yes" | "y" | "1")
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // ── clamp ──────────────────────────────────────────────────────
    #[test]
    fn clamp_within_range() {
        assert_eq!(clamp(5.0, 0.0, 10.0), 5.0);
    }
    #[test]
    fn clamp_below() {
        assert_eq!(clamp(-1.0, 0.0, 10.0), 0.0);
    }
    #[test]
    fn clamp_above() {
        assert_eq!(clamp(11.0, 0.0, 10.0), 10.0);
    }

    // ── safe_float ─────────────────────────────────────────────────
    #[test]
    fn safe_float_valid() {
        assert_eq!(safe_float(Some("42.5"), 0.0), 42.5);
    }
    #[test]
    fn safe_float_none() {
        assert_eq!(safe_float(None, 99.0), 99.0);
    }
    #[test]
    fn safe_float_unknown() {
        assert_eq!(safe_float(Some("unknown"), 0.0), 0.0);
    }
    #[test]
    fn safe_float_unavailable() {
        assert_eq!(safe_float(Some("unavailable"), 0.0), 0.0);
    }
    #[test]
    fn safe_float_empty() {
        assert_eq!(safe_float(Some(""), 0.0), 0.0);
    }
    #[test]
    fn safe_float_garbage() {
        assert_eq!(safe_float(Some("not_a_number"), 5.0), 5.0);
    }
    #[test]
    fn safe_float_none_lowercase() {
        assert_eq!(safe_float(Some("none"), 1.0), 1.0);
    }

    // ── pct_to_ha_brightness ───────────────────────────────────────
    #[test]
    fn pct_0() {
        assert_eq!(pct_to_ha_brightness(0.0), 0);
    }
    #[test]
    fn pct_100() {
        assert_eq!(pct_to_ha_brightness(100.0), 255);
    }
    #[test]
    fn pct_50() {
        assert_eq!(pct_to_ha_brightness(50.0), 128);
    }
    #[test]
    fn pct_over_100_clamped() {
        assert_eq!(pct_to_ha_brightness(110.0), 255);
    }
    #[test]
    fn pct_negative_clamped() {
        assert_eq!(pct_to_ha_brightness(-5.0), 0);
    }

    // ── ha_brightness_to_pct ───────────────────────────────────────
    #[test]
    fn bri_to_pct_255() {
        assert!((ha_brightness_to_pct(255) - 100.0).abs() < 0.01);
    }
    #[test]
    fn bri_to_pct_0() {
        assert!((ha_brightness_to_pct(0)).abs() < 0.01);
    }

    // ── kelvin_to_mired ────────────────────────────────────────────
    #[test]
    fn kelvin_to_mired_3000() {
        assert_eq!(kelvin_to_mired(3000), 333);
    }
    #[test]
    fn kelvin_to_mired_6500() {
        assert_eq!(kelvin_to_mired(6500), 154);
    }
    #[test]
    fn kelvin_to_mired_zero() {
        assert_eq!(kelvin_to_mired(0), 0);
    }
    #[test]
    fn kelvin_to_mired_negative() {
        assert_eq!(kelvin_to_mired(-100), 0);
    }

    // ── linmap ─────────────────────────────────────────────────────
    #[test]
    fn linmap_mid() {
        assert!((linmap(5.0, 0.0, 10.0, 0.0, 100.0) - 50.0).abs() < 0.01);
    }
    #[test]
    fn linmap_clamped_below() {
        assert!((linmap(-1.0, 0.0, 10.0, 0.0, 100.0) - 0.0).abs() < 0.01);
    }
    #[test]
    fn linmap_clamped_above() {
        assert!((linmap(15.0, 0.0, 10.0, 0.0, 100.0) - 100.0).abs() < 0.01);
    }
    #[test]
    fn linmap_degenerate() {
        assert_eq!(linmap(5.0, 5.0, 5.0, 10.0, 20.0), 20.0);
        assert_eq!(linmap(4.0, 5.0, 5.0, 10.0, 20.0), 10.0);
    }

    // ── smoothstep ─────────────────────────────────────────────────
    #[test]
    fn smoothstep_endpoints() {
        assert!((smoothstep(0.0)).abs() < 0.001);
        assert!((smoothstep(1.0) - 1.0).abs() < 0.001);
    }
    #[test]
    fn smoothstep_mid() {
        assert!((smoothstep(0.5) - 0.5).abs() < 0.001);
    }
    #[test]
    fn smoothstep_clamped() {
        assert!((smoothstep(-1.0)).abs() < 0.001);
        assert!((smoothstep(2.0) - 1.0).abs() < 0.001);
    }

    // ── lerp ───────────────────────────────────────────────────────
    #[test]
    fn lerp_basic() {
        assert!((lerp(0.0, 10.0, 0.5) - 5.0).abs() < 0.001);
    }
    #[test]
    fn lerp_extrapolate() {
        // t not clamped
        assert!((lerp(0.0, 10.0, 2.0) - 20.0).abs() < 0.001);
    }

    // ── as_bool ────────────────────────────────────────────────────
    #[test]
    fn as_bool_true_values() {
        for s in &["true", "on", "yes", "y", "1", "ON", "True", "YES"] {
            assert!(as_bool(Some(s)), "expected true for {s:?}");
        }
    }
    #[test]
    fn as_bool_false_values() {
        for s in &["false", "off", "no", "0", ""] {
            assert!(!as_bool(Some(s)), "expected false for {s:?}");
        }
        assert!(!as_bool(None));
    }
}
