// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

pub const VERSION_MAJOR: u8 = 1;
pub const VERSION_MINOR: u8 = 99;
pub const VERSION_REVISION: u16 = 1;

pub mod error;
pub mod util;
pub mod vl1;
pub mod vl2;

mod event;
mod networkhypervisor;

pub use event::Event;
pub use networkhypervisor::{Interface, NetworkHypervisor};
pub use vl1::protocol::{PacketBuffer, PooledPacketBuffer};
