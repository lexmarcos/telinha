//! Design system springs (DESIGN.md, "Movimento"). Colors, sizes and short
//! durations live in ui/tokens.slint; springs live here because the app
//! animates them, frame by frame.

/// Apple-style springs: damping and response (seconds).
#[derive(Debug, Clone, Copy)]
pub struct SpringParams {
    pub damping: f32,
    pub response: f32,
}

/// Panel opening and closing, panel switch, segmented control marker.
pub const UI: SpringParams = SpringParams { damping: 1.0, response: 0.32 };
/// Shrink on press and come back.
pub const PRESS: SpringParams = SpringParams { damping: 1.0, response: 0.12 };
/// The red ring arriving when the stream starts (the only bounce).
pub const TALLY: SpringParams = SpringParams { damping: 0.7, response: 0.45 };
/// New content appearing when the panel switches.
pub const FADE: SpringParams = SpringParams { damping: 1.0, response: 0.22 };
