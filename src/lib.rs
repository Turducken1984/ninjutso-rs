//! Linux configuration tools for Ninjutso mice.
//!
//! The transport is raw hidraw feature reports, so there is no hidapi or libusb
//! dependency -- just `ioctl` from libc. [`device::Device`] is the single entry
//! point; the CLI, the GTK front end and the battery tray all sit on top of it.

pub mod device;
pub mod firmware;
pub mod protocol;
pub mod transport;

#[cfg(feature = "gui")]
pub mod app;

#[cfg(feature = "tray")]
pub mod icons;

#[cfg(feature = "tray")]
pub mod tray;

/// Everything that can go wrong talking to a mouse.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("No Ninjutso config interface found. Is the receiver plugged in?")]
    DeviceNotFound,

    #[error("{path} is not readable by you -- install 70-ninjutso.rules and replug the receiver.")]
    PermissionDenied { path: String },

    #[error("device did not answer command 0x{0:02x}")]
    Timeout(u8),

    /// The receiver is answering, but the mouse is not on the air.
    #[error("mouse is not connected to the receiver -- move or click it to wake it")]
    Offline,

    /// The connected hardware does not implement this command.
    #[error("{0}")]
    Unsupported(String),

    #[error("{0}")]
    Protocol(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// True for the two failures a user can actually fix (replug, or install
    /// the udev rule), which the front ends report differently from a bug.
    pub fn is_access_problem(&self) -> bool {
        matches!(self, Error::DeviceNotFound | Error::PermissionDenied { .. })
    }

    /// True when the receiver is fine and only the mouse is asleep, which the
    /// user fixes by touching the mouse rather than by touching the software.
    pub fn is_offline(&self) -> bool {
        matches!(self, Error::Offline)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
