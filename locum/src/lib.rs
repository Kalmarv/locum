//! locum: a stand-in d3d11.dll and dxgi.dll. Every Direct3D 11 call the game makes passes through it to the real
//! runtime, to be recorded, read back or changed.

mod vtables;
