//! nvapi64.dll beside locum.dll for Direct3D 12 games, loaded by locum before the game creates a device. It exports
//! nothing: a game (or NVIDIA Aftermath) loading NVAPI by name gets this module and finds neither
//! `nvapi_QueryInterface` nor `nvapi_Direct_GetMethod`, as on another vendor's GPU. NVAPI's Direct3D 12 functions take
//! the driver's own device and lists, which locum's wrappers are not; given one, the driver reads its internals and
//! crashes.
