#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

// When building on unix, also include windows module for compile checking / testing where possible,
// but re-export from unix.
#[cfg(not(windows))]
pub mod windows;

#[cfg(not(windows))]
pub use unix::*;
pub use windows::quote_powershell_literal;

#[cfg(windows)]
pub use windows::*;
