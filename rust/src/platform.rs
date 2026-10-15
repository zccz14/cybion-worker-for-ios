#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "ios")]
pub use ios::*;

#[cfg(not(target_os = "ios"))]
mod stub;
#[cfg(not(target_os = "ios"))]
pub use stub::*;
