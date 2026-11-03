//! locum's capture format, and what capturing needs that is the same for every graphics API: the database and its
//! tables ([`db`]), the blob store ([`blobs`]), DXGI formats and the canonical layout of resource contents
//! ([`format`], [`layout`]), hashes ([`hash`]), call stacks ([`stacks`]), the background writer ([`writer`]) and
//! DXBC shader reflection ([`rdef`]).

pub mod blobs;
pub mod db;
pub mod format;
pub mod hash;
pub mod layout;
pub mod rdef;
pub mod stacks;
pub mod writer;
