//! VibingLUT engine: UI-agnostic photo loading, GPU grading, LUT baking and
//! scopes. Front-ends talk to it through [`gpu::spawn`] jobs/results.

pub mod chart;
pub mod gpu;
pub mod grade;
pub mod lab;
pub mod lut;
pub mod matching;
pub mod photo;
pub mod preset;
pub mod profiles;
pub mod scopes;
