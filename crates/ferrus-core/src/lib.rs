//! ferrus-core: platform logic for Ferrus.
//!
//! Everything that touches disks, images or the system lives here,
//! deliberately free of GUI dependencies so the privileged helper and
//! headless tests can use it too.

pub mod bcd;
pub mod client;
pub mod device;
pub mod iso;
pub mod protocol;
pub mod unattend;
pub mod vhdx;
pub mod write;
