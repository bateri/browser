//! What bateri and the browser helper say to each other.
//!
//! - `message`: the control conversation (commands to the helper, events back), plain data.
//! - `channel`: the socket it runs on, the handshake, and both ends' typed handles.
//! - `mach`: frames of the page, one IOSurface per mach message.
//! - `manifest`: the release notice bateri reads to find a newer helper.
//! - `import` (feature `wgpu`): a frame as a wgpu texture.
//!
//! bateri takes this crate by `rev`; the helper builds the other end. Nothing here depends on the
//! engine.

pub mod channel;
pub mod mach;
pub mod manifest;
pub mod message;

#[cfg(feature = "wgpu")]
pub mod import;
#[cfg(feature = "wgpu")]
pub use wgpu;

pub use channel::{Client, Connection, Launch, Reader, Writer};
pub use mach::{Frame, KIND_POPUP, KIND_VIEW, Receiver, Sender, Sent, Surface};
pub use message::{Command, Event, PROTOCOL};
