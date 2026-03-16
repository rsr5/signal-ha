use serde::{Deserialize, Serialize};

/// Why a row has been flagged for deletion.
///
/// Stored as a `TINYINT UNSIGNED` in MySQL / `INTEGER` in SQLite.
/// `NULL` means unflagged (keep).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum DeletionReason {
    /// Entity updates faster than useful — keeping a downsampled subset.
    HighFrequency = 1,
    /// Intermediate value between meaningful transitions.
    Superseded = 2,
    /// Entity type/domain is not useful for historical analysis.
    LowValue = 3,
    /// Record older than useful retention window for this entity class.
    Aged = 4,
    /// Agent made a case-specific judgement call.
    AgentDecision = 5,
}

impl DeletionReason {
    /// All variants, for iteration.
    pub const ALL: &'static [DeletionReason] = &[
        DeletionReason::HighFrequency,
        DeletionReason::Superseded,
        DeletionReason::LowValue,
        DeletionReason::Aged,
        DeletionReason::AgentDecision,
    ];

    /// Human-readable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeletionReason::HighFrequency => "HighFrequency",
            DeletionReason::Superseded => "Superseded",
            DeletionReason::LowValue => "LowValue",
            DeletionReason::Aged => "Aged",
            DeletionReason::AgentDecision => "AgentDecision",
        }
    }

    /// Convert from the integer stored in the database.
    pub fn from_u8(v: u8) -> Option<DeletionReason> {
        match v {
            1 => Some(DeletionReason::HighFrequency),
            2 => Some(DeletionReason::Superseded),
            3 => Some(DeletionReason::LowValue),
            4 => Some(DeletionReason::Aged),
            5 => Some(DeletionReason::AgentDecision),
            _ => None,
        }
    }

    /// The integer value stored in the database.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parse from a string label (case-insensitive).
    pub fn from_str(s: &str) -> Option<DeletionReason> {
        match s.to_ascii_lowercase().as_str() {
            "highfrequency" | "high_frequency" => Some(DeletionReason::HighFrequency),
            "superseded" => Some(DeletionReason::Superseded),
            "lowvalue" | "low_value" => Some(DeletionReason::LowValue),
            "aged" => Some(DeletionReason::Aged),
            "agentdecision" | "agent_decision" => Some(DeletionReason::AgentDecision),
            _ => None,
        }
    }
}

impl std::fmt::Display for DeletionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for reason in DeletionReason::ALL {
            assert_eq!(DeletionReason::from_u8(reason.as_u8()), Some(*reason));
        }
    }

    #[test]
    fn invalid_returns_none() {
        assert_eq!(DeletionReason::from_u8(0), None);
        assert_eq!(DeletionReason::from_u8(6), None);
        assert_eq!(DeletionReason::from_u8(255), None);
    }

    #[test]
    fn display() {
        assert_eq!(DeletionReason::HighFrequency.to_string(), "HighFrequency");
        assert_eq!(DeletionReason::AgentDecision.to_string(), "AgentDecision");
    }
}
