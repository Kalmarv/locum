//! locum's capture format, and what capturing needs that is the same for every graphics API: the database and its
//! tables ([`db`]), the blob store ([`blobs`]), DXGI formats and the canonical layout of resource contents
//! ([`format`], [`layout`]), what a map wrote ([`maps`]), hashes ([`hash`]), call stacks ([`stacks`]), the background writer ([`writer`]),
//! DXBC shader reflection ([`rdef`]) and atomic order dependence ([`dxbc`]), the rules that change what is drawn ([`rules`]), reading contents back out: texels ([`texels`]) and DDS files ([`dds`]), and the table of a game's values that locum reads and sets for a run ([`values`]).

pub mod blobs;
pub mod cost;
pub mod d3d12;
pub mod db;
pub mod dds;
pub mod dxbc;
pub mod format;
pub mod hash;
pub mod index;
pub mod layout;
pub mod maps;
pub mod rdef;
pub mod rules;
pub mod stacks;
pub mod texels;
pub mod tiles;
pub mod values;
pub mod writer;
