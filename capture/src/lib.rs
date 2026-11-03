//! locum's capture format, and what capturing needs that is the same for every graphics API: the database and its
//! tables ([`db`]), the blob store ([`blobs`]), DXGI formats and the canonical layout of resource contents
//! ([`format`], [`layout`]), hashes ([`hash`]), call stacks ([`stacks`]), the background writer ([`writer`]),
//! DXBC shader reflection ([`rdef`]), the rules that change what is drawn ([`rules`]), and reading contents back out: texels ([`texels`]) and DDS files ([`dds`]).

pub mod blobs;
pub mod cost;
pub mod db;
pub mod dds;
pub mod format;
pub mod hash;
pub mod layout;
pub mod rdef;
pub mod rules;
pub mod stacks;
pub mod texels;
pub mod writer;
