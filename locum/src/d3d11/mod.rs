//! The Direct3D 11 front end: the device and its contexts, shader identity, per-stage bindings, capture and
//! overrides. Everything API-neutral (the capture format, blob store, formats, rules) is in `locum-capture`.

pub mod capture;
pub mod device;
pub mod overrides;
pub mod recorder;
pub mod registry;
pub mod shaders;
pub mod stages;
