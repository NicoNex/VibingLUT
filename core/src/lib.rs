//! VibingLUT engine: UI-agnostic photo loading, GPU grading, LUT baking and
//! scopes. Front-ends talk to it through [`gpu::spawn`] jobs/results.

pub mod gpu;
pub mod grade;
pub mod lut;
pub mod photo;
pub mod scopes;
