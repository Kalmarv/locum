//! d3d11.dll beside the game: no code of its own, every export forwards to the same name in locum.dll (build.rs).
// The link takes its export table from the forwarders' .exp and says so (LNK4052, rustc's empty .def ignored).
#![allow(linker_messages)]
