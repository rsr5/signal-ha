/// Entity allowlist with wildcard pattern matching.
///
/// Patterns support `*` as a glob (matches any sequence of characters).
///
/// # Examples
///
/// ```
/// use signal_ha_recorder::EntityFilter;
///
/// let filter = EntityFilter::new(vec![
///     "sensor.*".into(),
///     "light.porch_*".into(),
///     "binary_sensor.front_door".into(),
/// ]);
///
/// assert!(filter.matches("sensor.temperature"));
/// assert!(filter.matches("sensor.humidity"));
/// assert!(filter.matches("light.porch_left"));
/// assert!(filter.matches("binary_sensor.front_door"));
/// assert!(!filter.matches("switch.garage"));
/// assert!(!filter.matches("light.kitchen"));
/// ```
pub struct EntityFilter {
    patterns: Vec<String>,
}

impl EntityFilter {
    /// Create a filter from a list of patterns.
    ///
    /// Each pattern can contain `*` wildcards. An empty list matches nothing.
    pub fn new(patterns: Vec<String>) -> Self {
        Self { patterns }
    }

    /// Create a filter that matches everything.
    pub fn allow_all() -> Self {
        Self {
            patterns: vec!["*".into()],
        }
    }

    /// Check if an entity ID matches any pattern in the allowlist.
    pub fn matches(&self, entity_id: &str) -> bool {
        self.patterns.iter().any(|p| glob_match(p, entity_id))
    }
}

/// Simple glob matching — only `*` is special (matches any sequence of chars).
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();

    if parts.len() == 1 {
        // No wildcard — exact match
        return pattern == text;
    }

    let mut pos = 0;

    // First part must match at start
    if let Some(first) = parts.first() {
        if !first.is_empty() {
            if !text.starts_with(first) {
                return false;
            }
            pos = first.len();
        }
    }

    // Last part must match at end
    if let Some(last) = parts.last() {
        if !last.is_empty() {
            if !text.ends_with(last) {
                return false;
            }
            // Check for overlap between start constraint and end constraint
            if pos + last.len() > text.len() {
                return false;
            }
        }
    }

    // Middle parts must appear in order
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        if let Some(idx) = text[pos..].find(part) {
            pos += idx + part.len();
        } else {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        let f = EntityFilter::new(vec!["sensor.temp".into()]);
        assert!(f.matches("sensor.temp"));
        assert!(!f.matches("sensor.humidity"));
    }

    #[test]
    fn trailing_wildcard() {
        let f = EntityFilter::new(vec!["sensor.*".into()]);
        assert!(f.matches("sensor.temp"));
        assert!(f.matches("sensor.humidity"));
        assert!(!f.matches("light.porch"));
    }

    #[test]
    fn leading_wildcard() {
        let f = EntityFilter::new(vec!["*.temp".into()]);
        assert!(f.matches("sensor.temp"));
        assert!(!f.matches("sensor.humidity"));
    }

    #[test]
    fn middle_wildcard() {
        let f = EntityFilter::new(vec!["light.*_left".into()]);
        assert!(f.matches("light.porch_left"));
        assert!(f.matches("light.kitchen_left"));
        assert!(!f.matches("light.porch_right"));
    }

    #[test]
    fn star_matches_all() {
        let f = EntityFilter::new(vec!["*".into()]);
        assert!(f.matches("sensor.anything"));
        assert!(f.matches("light.whatever"));
    }

    #[test]
    fn multiple_patterns() {
        let f = EntityFilter::new(vec!["sensor.*".into(), "light.*".into()]);
        assert!(f.matches("sensor.temp"));
        assert!(f.matches("light.porch"));
        assert!(!f.matches("switch.garage"));
    }

    #[test]
    fn empty_filter_matches_nothing() {
        let f = EntityFilter::new(vec![]);
        assert!(!f.matches("sensor.temp"));
    }

    #[test]
    fn allow_all() {
        let f = EntityFilter::allow_all();
        assert!(f.matches("sensor.temp"));
        assert!(f.matches("anything"));
    }
}
