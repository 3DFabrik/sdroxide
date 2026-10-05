//! NOAA APT status, for the picture panel.

use serde::{Deserialize, Serialize};

/// What the APT decoder is making of the pass.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AptStatus {
    pub receiving: bool,
    pub lines_a: u16,
    pub lines_b: u16,
    /// Envelope level, 0..1, for a tuning hint.
    pub signal: f32,
    pub saved: u32,
}

impl Default for AptStatus {
    fn default() -> Self {
        AptStatus { receiving: false, lines_a: 0, lines_b: 0, signal: 0.0, saved: 0 }
    }
}
