//! Rust support for clipping textured OSGB meshes against XY polygon prisms.

mod mesh;
pub mod osgb;

pub use mesh::Boundary;

mod vector;
pub use vector::zip_layers;

mod crs;
pub use crs::Reference;

mod job;
pub use job::{
    Preflight, Progress, Report, Request, StagedClip, compute_staged, preflight,
    preflight_cancelable, run, run_with_pool,
};
