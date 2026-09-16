//! What [`crate::device::Device`] talks through: 16-byte HID feature reports.
//!
//! Splitting this out from the device logic is what makes the protocol layer
//! testable at all. Everything above this trait -- encoding, read-back
//! confirmation, timeout classification, the whole of `status()` -- runs
//! unchanged against [`Fake`], so a wrong DPI encoding or a mis-read battery is
//! caught by `cargo test` rather than by plugging a mouse in.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::Result;

/// One request/response channel to a device.
///
/// `Send`, because the GUI hands its device to a worker thread.
pub trait Transport: Send {
    /// Write a feature report (`HIDIOCSFEATURE`).
    fn set_feature(&self, report_id: u8, payload: &[u8]) -> Result<()>;

    /// Read the report the device prepared for the last request
    /// (`HIDIOCGFEATURE`). The returned buffer leads with the report ID.
    fn get_feature(&self, report_id: u8, length: usize) -> Result<Vec<u8>>;

    /// Wait for the device to catch up.
    ///
    /// Time is part of the seam so a fake can run a virtual clock: the retry
    /// budget is then spent exactly as it would be on hardware, in no
    /// wall-clock time at all, and a test can assert on how much of it went.
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

// _IOC(dir = READ|WRITE, type = 'H', nr, size)
const IOC_RW: u32 = 3;

const fn ioc(nr: u32, size: u32) -> libc::c_ulong {
    ((IOC_RW << 30) | (size << 16) | ((b'H' as u32) << 8) | nr) as libc::c_ulong
}

pub const fn hidiocsfeature(size: u32) -> libc::c_ulong {
    ioc(0x06, size)
}

pub const fn hidiocgfeature(size: u32) -> libc::c_ulong {
    ioc(0x07, size)
}

/// The real thing: a hidraw node driven by ioctl, with no hidapi in between.
#[derive(Debug)]
pub struct Hidraw {
    fd: OwnedFd,
}

impl Hidraw {
    pub fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }
}

impl Transport for Hidraw {
    fn set_feature(&self, report_id: u8, payload: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(payload.len() + 1);
        buf.push(report_id);
        buf.extend_from_slice(payload);
        let request = hidiocsfeature(buf.len() as u32);
        // SAFETY: `buf` is a live allocation of exactly the length encoded in
        // the ioctl request, which is what HIDIOCSFEATURE reads.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), request, buf.as_mut_ptr()) };
        if rc < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn get_feature(&self, report_id: u8, length: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; length + 1];
        buf[0] = report_id;
        let request = hidiocgfeature(buf.len() as u32);
        // SAFETY: as above; HIDIOCGFEATURE writes at most `buf.len()` bytes.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), request, buf.as_mut_ptr()) };
        if rc < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(buf)
    }
}

#[cfg(test)]
pub use fake::Fake;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_requests_match_the_kernel_definitions() {
        // HIDIOCSFEATURE(16) / HIDIOCGFEATURE(16) as the hidraw uapi header
        // spells them: _IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06|0x07, len).
        assert_eq!(hidiocsfeature(16), 0xC010_4806);
        assert_eq!(hidiocgfeature(16), 0xC010_4807);
    }
}

#[cfg(test)]
mod fake {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::Transport;
    use crate::protocol::{self as p, cmd};
    use crate::{Error, Result};

    /// Virtual time, as (origin, now). Starts the first time it is consulted,
    /// so a fake that is never asked about time never reads the real clock.
    type Clock = Option<(Instant, Instant)>;

    trait Started {
        fn started(&mut self) -> &mut (Instant, Instant);
    }

    impl Started for Clock {
        fn started(&mut self) -> &mut (Instant, Instant) {
            self.get_or_insert_with(|| {
                let origin = Instant::now();
                (origin, origin)
            })
        }
    }

    /// How a setter's arguments land in the state a getter reads back.
    enum Shape {
        /// The whole argument list is the value: `SET_POLLING_RATE [2]`.
        Whole,
        /// `args[0]` selects which value to write: `SET_DPI [stage, lo, hi, 0]`.
        Keyed,
    }

    /// Setter -> the getter that must then report the new value. This is the
    /// pairing the hardware implements and the one every setter asserts by
    /// reading back, so the fake has to honour it or the tests prove nothing.
    const LINKS: [(u8, u8, Shape); 8] = [
        (cmd::SET_DPI, cmd::DPI, Shape::Keyed),
        (cmd::SET_POLLING_RATE, cmd::POLLING_RATE, Shape::Whole),
        (cmd::SET_LIFT_OFF, cmd::LIFT_OFF, Shape::Whole),
        (cmd::SET_MOTION_SYNC, cmd::MOTION_SYNC, Shape::Whole),
        (cmd::SET_LIGHTING_STATE, cmd::LIGHTING_STATE, Shape::Whole),
        (cmd::SET_LIGHTING_MODE, cmd::LIGHTING_MODE, Shape::Whole),
        (cmd::SET_LIGHTING_COLOR, cmd::LIGHTING_COLOR, Shape::Whole),
        (
            cmd::SET_LIGHTING_BRIGHTNESS,
            cmd::LIGHTING_BRIGHTNESS,
            Shape::Whole,
        ),
    ];

    /// A scripted stand-in for a mouse.
    ///
    /// Holds the device's readable state keyed by `(command, argument)`, applies
    /// writes to it through [`LINKS`], and answers the last request it was given
    /// -- which is all the real protocol is. A command left out of the state is
    /// simply never answered, which is exactly how an unimplemented command and
    /// a sleeping mouse both present on the wire.
    #[derive(Debug, Default)]
    pub struct Fake {
        state: Mutex<HashMap<(u8, u8), Vec<u8>>>,
        /// Commands that go unanswered even though they have state -- a mouse
        /// that has gone to sleep behind a receiver that is still talking.
        silent: Mutex<HashSet<u8>>,
        pending: Mutex<Option<(u8, u8)>>,
        pub writes: Mutex<Vec<(u8, Vec<u8>)>>,
        /// Advanced only by the code under test asking to wait.
        clock: Mutex<Clock>,
    }

    impl Fake {
        /// A Ninjutso Ten as it actually answers: 1600 DPI across four stages,
        /// 1 kHz, lighting present, no brightness command.
        pub fn ten() -> Self {
            let fake = Self::default();
            fake.set(cmd::PROFILE, 0, &[1])
                .set(cmd::PAIRED_PRODUCT_ID, 0, &[0x20, 0xE0])
                .set(cmd::ACTIVE_DPI_STAGE, 0, &[0])
                .set(cmd::DPI_STAGE_COUNT, 0, &[4])
                .set(cmd::POLLING_RATE, 0, &[0])
                .set(cmd::LIFT_OFF, 0, &[1])
                .set(cmd::MOTION_SYNC, 0, &[1])
                .set(cmd::ANGLE_TUNING, 0, &[0])
                .set(cmd::SYSTEM_MODE, 0, &[0])
                .set(cmd::SLEEP_MINUTES, 0, &[2])
                .set(cmd::BATTERY_PERCENT, 0, &[73])
                .set(cmd::BATTERY_CHARGING, 0, &[0])
                .set(cmd::ONLINE, 0, &[1])
                .set(cmd::FIRMWARE, 0, &[0x03, 0x01, 0x00])
                .set(cmd::FIRMWARE, 1, &[0x07, 0x02, 0x00])
                .set(cmd::LIGHTING_STATE, 0, &[1])
                .set(cmd::LIGHTING_MODE, 0, &[1])
                .set(cmd::LIGHTING_COLOR, 0, &[0x36, 0xAD, 0x6A])
                .set(cmd::LIGHTING_SPEED, 0, &[10]);
            for stage in 0..4 {
                // 1600 DPI stepped: 1600/50 - 1 = 31.
                fake.set(cmd::DPI, stage, &[31, 0, 0]);
            }
            fake
        }

        /// A Sora V3, which takes 1-DPI steps and answers the brightness
        /// command the Ten does not implement.
        pub fn sora_v3() -> Self {
            let fake = Self::ten();
            fake.set(cmd::PAIRED_PRODUCT_ID, 0, &[0x10, 0xE0])
                .set(cmd::LIGHTING_BRIGHTNESS, 0, &[2]);
            for stage in 0..4 {
                // 36000 DPI direct -- past the Ten's 30000 ceiling on purpose.
                fake.set(cmd::DPI, stage, &[0xA0, 0x8C, 0]);
            }
            fake
        }

        pub fn set(&self, command: u8, arg: u8, value: &[u8]) -> &Self {
            self.state
                .lock()
                .unwrap()
                .insert((command, arg), value.to_vec());
            self
        }

        /// Stop answering `command`, as a mouse does when it goes to sleep.
        pub fn silence(&self, command: u8) -> &Self {
            self.silent.lock().unwrap().insert(command);
            self
        }

        /// How much time the code under test believes has passed.
        pub fn elapsed(&self) -> Duration {
            self.clock
                .lock()
                .unwrap()
                .map_or(Duration::ZERO, |(start, now)| now - start)
        }

        pub fn wrote(&self, command: u8) -> Option<Vec<u8>> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(written, _)| *written == command)
                .map(|(_, args)| args.clone())
        }
    }

    /// So a test can keep hold of the fake to inspect what was written to it
    /// while the device owns its own handle on the same one.
    impl Transport for std::sync::Arc<Fake> {
        fn set_feature(&self, report_id: u8, payload: &[u8]) -> Result<()> {
            (**self).set_feature(report_id, payload)
        }
        fn get_feature(&self, report_id: u8, length: usize) -> Result<Vec<u8>> {
            (**self).get_feature(report_id, length)
        }
        fn sleep(&self, duration: Duration) {
            (**self).sleep(duration);
        }
        fn now(&self) -> Instant {
            (**self).now()
        }
    }

    impl Transport for Fake {
        /// Advance the virtual clock instead of actually waiting.
        fn sleep(&self, duration: Duration) {
            self.clock.lock().unwrap().started().1 += duration;
        }

        fn now(&self) -> Instant {
            self.clock.lock().unwrap().started().1
        }

        fn set_feature(&self, report_id: u8, payload: &[u8]) -> Result<()> {
            if report_id == p::CONTROL_REPORT_ID {
                return Ok(());
            }
            if payload.len() != p::PAYLOAD_LEN {
                return Err(Error::Protocol("short request".into()));
            }
            let command = payload[0];
            let args = &payload[7..7 + payload[5] as usize];
            self.writes.lock().unwrap().push((command, args.to_vec()));

            if let Some((_, getter, shape)) =
                LINKS.iter().find(|(setter, _, _)| *setter == command)
            {
                let (key, value) = match shape {
                    Shape::Whole => (0, args),
                    Shape::Keyed => (args[0], &args[1..]),
                };
                self.state
                    .lock()
                    .unwrap()
                    .insert((*getter, key), value.to_vec());
            }
            // Whatever this was, the device now owes us an answer to it.
            *self.pending.lock().unwrap() = Some((command, args.first().copied().unwrap_or(0)));
            Ok(())
        }

        fn get_feature(&self, report_id: u8, length: usize) -> Result<Vec<u8>> {
            let mut buf = vec![0u8; length + 1];
            buf[0] = report_id;
            let Some((command, arg)) = *self.pending.lock().unwrap() else {
                return Ok(buf);
            };
            if self.silent.lock().unwrap().contains(&command) {
                return Ok(buf); // no reply: buf[1] stays 0, so it matches nothing
            }
            let state = self.state.lock().unwrap();
            let Some(value) = state.get(&(command, arg)) else {
                return Ok(buf);
            };
            buf[1] = command;
            let end = (8 + value.len()).min(buf.len());
            buf[8..end].copy_from_slice(&value[..end - 8]);
            Ok(buf)
        }
    }
}
