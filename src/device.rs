//! hidraw transport for Ninjutso devices -- no hidapi needed, just ioctl.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::protocol::{self as p, cmd};
use crate::{Error, Result};

const FIRST_READ_DELAY: Duration = Duration::from_millis(30);
const RETRY_READ_DELAY: Duration = Duration::from_millis(60);

/// How long a required read keeps asking before giving up.
///
/// Measured on a Ten: after a polling-rate write the device needs five attempts
/// (~270 ms) to answer, because changing the report rate renegotiates the 2.4 GHz
/// link; after a lighting write it needs three; every other read answers on the
/// first. The old fixed budget of four attempts could therefore never confirm a
/// rate change, and the setter reported failure on a write that had landed.
///
/// A duration rather than a count, so a slower link or another model does not need
/// the number re-tuned. It only bounds failure: a read that succeeds returns as
/// soon as the device answers.
const READ_BUDGET: Duration = Duration::from_millis(750);

// _IOC(dir = READ|WRITE, type = 'H', nr, size)
const IOC_RW: u32 = 3;

const fn ioc(nr: u32, size: u32) -> libc::c_ulong {
    ((IOC_RW << 30) | (size << 16) | ((b'H' as u32) << 8) | nr) as libc::c_ulong
}

const fn hidiocsfeature(size: u32) -> libc::c_ulong {
    ioc(0x06, size)
}

const fn hidiocgfeature(size: u32) -> libc::c_ulong {
    ioc(0x07, size)
}

/// The config channel is the vendor collection declaring report 6:
/// `06 01 ff  09 01  a1 01  85 06`
const CONFIG_COLLECTION: [u8; 9] = [0x06, 0x01, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x06];

/// One Ninjutso config interface as found in sysfs.
#[derive(Debug, Clone)]
pub struct Node {
    pub path: PathBuf,
    pub product_id: u16,
    pub name: String,
}

fn uevent(path: &Path) -> Vec<(String, String)> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn lookup<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Every Ninjutso config interface currently attached.
///
/// A receiver exposes several hidraw nodes -- only the one whose report
/// descriptor carries the vendor collection above answers config commands.
pub fn find_nodes(vendor_id: u16) -> Vec<Node> {
    let mut entries: Vec<PathBuf> = fs::read_dir("/sys/class/hidraw")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    let mut found = Vec::new();
    for node in entries {
        let fields = uevent(&node.join("device/uevent"));
        // HID_ID is "bus:vendor:product", all hex, e.g. 0003:0000093A:0000EB01.
        let Some(hid_id) = lookup(&fields, "HID_ID") else {
            continue;
        };
        let parts: Vec<&str> = hid_id.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let (Ok(vid), Ok(pid)) = (
            u32::from_str_radix(parts[1], 16),
            u32::from_str_radix(parts[2], 16),
        ) else {
            continue;
        };
        if vid as u16 != vendor_id {
            continue;
        }
        let Ok(descriptor) = fs::read(node.join("device/report_descriptor")) else {
            continue;
        };
        if !descriptor
            .windows(CONFIG_COLLECTION.len())
            .any(|window| window == CONFIG_COLLECTION)
        {
            continue;
        }
        let Some(name) = node.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        found.push(Node {
            path: PathBuf::from("/dev").join(name),
            product_id: pid as u16,
            name: lookup(&fields, "HID_NAME")
                .unwrap_or("Ninjutso device")
                .to_string(),
        });
    }
    found
}

/// Everything [`Device::status`] reads in one pass.
#[derive(Debug, Clone)]
pub struct Status {
    pub name: String,
    pub path: PathBuf,
    pub product_id: u16,
    pub effective_product_id: u16,
    pub direct: bool,
    pub profile: u8,
    pub dpi_stage: usize,
    pub dpi_stages: Vec<f64>,
    pub dpi: Option<f64>,
    pub polling_rate: u32,
    pub lift_off: Option<&'static str>,
    pub motion_sync: bool,
    pub angle_tuning: bool,
    pub system_mode: Option<&'static str>,
    pub sleep_minutes: Option<u8>,
    pub battery: u8,
    pub charging: Option<bool>,
    pub is_receiver: bool,
    /// (part, version), ordered mouse-then-receiver like the Python dict was.
    pub firmware: Vec<(&'static str, String)>,
    pub lighting: Option<Lighting>,
}

#[derive(Debug, Clone)]
pub struct Lighting {
    pub mode: &'static str,
    pub color: Option<String>,
    pub speed: Option<u8>,
    pub brightness: Option<u8>,
}

/// One open Ninjutso config interface.
pub struct Device {
    fd: OwnedFd,
    pub path: PathBuf,
    pub product_id: u16,
    pub name: String,
    profile: u8,
    effective_pid: Option<u16>,
}

impl Device {
    /// Open the first config interface found.
    pub fn open() -> Result<Self> {
        let node = find_nodes(p::VENDOR_ID)
            .into_iter()
            .next()
            .ok_or(Error::DeviceNotFound)?;
        Self::open_node(node)
    }

    /// Open a specific hidraw path, e.g. from `--device`.
    pub fn open_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        // A path given by hand may be any hidraw node, so identify it if we can
        // rather than assuming it is the one autodetect would have picked.
        let node = find_nodes(p::VENDOR_ID)
            .into_iter()
            .find(|node| node.path == path)
            .unwrap_or(Node {
                path,
                product_id: 0,
                name: "Ninjutso device".to_string(),
            });
        Self::open_node(node)
    }

    fn open_node(node: Node) -> Result<Self> {
        let fd = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node.path)
        {
            Ok(file) => OwnedFd::from(file),
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                return Err(Error::PermissionDenied {
                    path: node.path.display().to_string(),
                })
            }
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            fd,
            path: node.path,
            product_id: node.product_id,
            name: node.name,
            profile: 1,
            effective_pid: None,
        })
    }

    // -- raw feature reports -------------------------------------------------

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

    // -- protocol ------------------------------------------------------------

    /// Send a command and wait for the matching reply.
    ///
    /// The device answers asynchronously and will hand back a stale reply if
    /// asked too soon, so each attempt re-sends and re-reads until the reply's
    /// command byte matches.
    pub fn read(&self, command: u8, args: &[u8]) -> Result<Vec<u8>> {
        let deadline = Instant::now() + READ_BUDGET;
        let mut delay = FIRST_READ_DELAY;
        loop {
            if let Some(value) = self.attempt(command, args, delay)? {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(command));
            }
            delay = RETRY_READ_DELAY;
        }
    }

    /// One request/response round trip; `None` when the reply is not ours.
    fn attempt(&self, command: u8, args: &[u8], delay: Duration) -> Result<Option<Vec<u8>>> {
        let profile = if p::is_profile_command(command) {
            self.profile
        } else {
            0
        };
        let request = p::build_request(command, profile, args)?;
        self.set_feature(p::REPORT_ID, &request)?;
        sleep(delay);
        let buf = self.get_feature(p::REPORT_ID, p::PAYLOAD_LEN)?;
        Ok(p::response_value(&buf, command).map(<[u8]>::to_vec))
    }

    /// Read a command the device may not implement; `None` if unsupported.
    ///
    /// Exactly one attempt: an unanswered read is precisely how an unimplemented
    /// command presents, so retrying only slows detection. Never use this to
    /// confirm a write -- see [`READ_BUDGET`].
    pub fn read_optional(&self, command: u8, args: &[u8]) -> Option<Vec<u8>> {
        self.attempt(command, args, FIRST_READ_DELAY).ok().flatten()
    }

    pub fn write(&self, command: u8, args: &[u8]) -> Result<()> {
        let request = p::build_request(command, self.profile, args)?;
        self.set_feature(p::REPORT_ID, &request)
    }

    pub fn set_control(&self, resume: bool) -> Result<()> {
        self.set_feature(p::CONTROL_REPORT_ID, &p::control_payload(resume))?;
        sleep(Duration::from_millis(30));
        Ok(())
    }

    // -- high level ----------------------------------------------------------

    pub fn is_receiver(&self) -> bool {
        p::TEN_RECEIVER_IDS.contains(&self.product_id) || self.product_id == 0xEB02
    }

    /// PID of the mouse actually paired to this receiver.
    pub fn effective_product_id(&mut self) -> Result<u16> {
        if let Some(pid) = self.effective_pid {
            return Ok(pid);
        }
        let pid = if self.is_receiver() {
            let value = self.read(cmd::PAIRED_PRODUCT_ID, &[])?;
            ((value[1] as u16) << 8) | value[0] as u16
        } else {
            self.product_id
        };
        self.effective_pid = Some(pid);
        Ok(pid)
    }

    /// Sora V3 uses 1-DPI steps and a different feature set; TEN does not.
    pub fn direct(&mut self) -> Result<bool> {
        Ok(p::SORA_V3_IDS.contains(&self.effective_product_id()?))
    }

    pub fn status(&mut self) -> Result<Status> {
        self.profile = match self.read(cmd::PROFILE, &[])?[0] {
            0 => 1,
            value => value,
        };
        let direct = self.direct()?;
        let stage = self.read(cmd::ACTIVE_DPI_STAGE, &[])?[0] as usize;
        let dpi_now = self.read(cmd::DPI, &[stage as u8])?;
        let stage_count = self
            .read_optional(cmd::DPI_STAGE_COUNT, &[])
            .map_or(1, |value| value[0] as usize)
            .clamp(1, 4);

        let mut stages = Vec::new();
        for index in 0..stage_count {
            let value = if index == stage {
                Some(dpi_now.clone())
            } else {
                self.read_optional(cmd::DPI, &[index as u8])
            };
            let Some(value) = value else { break };
            stages.push(p::decode_dpi(value[0], value[1], value[2], direct));
        }

        let battery = self
            .read_optional(cmd::BATTERY_PERCENT, &[])
            .map_or(0, |value| value[0]);
        let charging = self
            .read_optional(cmd::BATTERY_CHARGING, &[])
            .map(|value| value[0] != 0);
        let lod = self.read_optional(cmd::LIFT_OFF, &[]);
        let system = self.read_optional(cmd::SYSTEM_MODE, &[]);

        let mut firmware = Vec::new();
        if let Some(value) = self.read_optional(cmd::FIRMWARE, &[0]) {
            firmware.push(("mouse", p::decode_firmware(&value)));
        }
        if self.is_receiver() {
            if let Some(value) = self.read_optional(cmd::FIRMWARE, &[1]) {
                firmware.push(("receiver", p::decode_firmware(&value)));
            }
        }

        let lighting = if self.is_receiver() {
            self.read_lighting()?
        } else {
            None
        };

        Ok(Status {
            name: self.name.clone(),
            path: self.path.clone(),
            product_id: self.product_id,
            effective_product_id: self.effective_product_id()?,
            direct,
            profile: self.profile,
            dpi_stage: stage,
            dpi: stages.get(stage).copied(),
            dpi_stages: stages,
            polling_rate: p::decode_polling(self.read(cmd::POLLING_RATE, &[])?[0]),
            lift_off: lod
                .as_ref()
                .and_then(|value| p::LOD_VALUES.get(value[0] as usize).copied()),
            motion_sync: self
                .read_optional(cmd::MOTION_SYNC, &[])
                .is_some_and(|value| value[0] != 0),
            angle_tuning: self
                .read_optional(cmd::ANGLE_TUNING, &[])
                .is_some_and(|value| value[0] != 0),
            system_mode: system
                .as_ref()
                .and_then(|value| p::SYSTEM_MODES.get(value[0] as usize).copied()),
            sleep_minutes: self
                .read_optional(cmd::SLEEP_MINUTES, &[])
                .map(|value| value[0]),
            battery: battery.min(100),
            charging,
            is_receiver: self.is_receiver(),
            firmware,
            lighting,
        })
    }

    /// Battery percent and charging flag -- two round-trips, ~60 ms.
    ///
    /// Deliberately avoids [`Device::status`], which costs 21 round-trips and
    /// wakes the 2.4 GHz link for far longer than a tray poll needs.
    pub fn battery(&self) -> (Option<u8>, Option<bool>) {
        let percent = self
            .read_optional(cmd::BATTERY_PERCENT, &[])
            .map(|value| value[0].min(100));
        let charging = self
            .read_optional(cmd::BATTERY_CHARGING, &[])
            .map(|value| value[0] != 0);
        (percent, charging)
    }

    pub fn read_lighting(&mut self) -> Result<Option<Lighting>> {
        let Some(state) = self.read_optional(cmd::LIGHTING_STATE, &[]) else {
            return Ok(None);
        };
        let mode = self.read_optional(cmd::LIGHTING_MODE, &[]);
        let speed = self.read_optional(cmd::LIGHTING_SPEED, &[]);
        let colour = self.read_optional(cmd::LIGHTING_COLOR, &[]);
        // Command 48 is unimplemented on the TEN -- it times out exactly like a
        // nonexistent command, same as optical_engine (50). Sora V3 only.
        let bright = if self.direct()? {
            self.read_optional(cmd::LIGHTING_BRIGHTNESS, &[])
        } else {
            None
        };

        const MODE_MAP: [&str; 4] = ["", "Static", "Cycling", "Wave"];
        let selected = if state[0] == 0 {
            "Off"
        } else {
            mode.as_ref()
                .and_then(|value| MODE_MAP.get(value[0] as usize).copied())
                .filter(|name| !name.is_empty())
                .unwrap_or("Static")
        };

        Ok(Some(Lighting {
            mode: selected,
            color: colour.map(|c| p::rgb_to_hex(c[0], c[1], c[2])),
            speed: speed.map(|value| 20u8.saturating_sub(value[0])),
            brightness: bright.map(|value| value[0].saturating_mul(25)),
        }))
    }

    // -- setters (each verifies by reading back) -----------------------------

    pub fn set_dpi(&mut self, dpi: u32, stage: Option<usize>) -> Result<f64> {
        let stage = match stage {
            Some(stage) => stage,
            None => self.read(cmd::ACTIVE_DPI_STAGE, &[])?[0] as usize,
        };
        let direct = self.direct()?;
        let mut args = vec![stage as u8];
        args.extend_from_slice(&p::encode_dpi(dpi, direct)?);
        self.write(cmd::SET_DPI, &args)?;
        sleep(Duration::from_millis(30));
        let value = self.read(cmd::DPI, &[stage as u8])?;
        Ok(p::decode_dpi(value[0], value[1], value[2], direct))
    }

    pub fn set_polling_rate(&mut self, rate: u32) -> Result<u32> {
        self.write(cmd::SET_POLLING_RATE, &[p::encode_polling(rate)?])?;
        sleep(Duration::from_millis(30));
        Ok(p::decode_polling(self.read(cmd::POLLING_RATE, &[])?[0]))
    }

    pub fn set_lift_off(&mut self, value: &str) -> Result<&'static str> {
        let index = p::LOD_VALUES
            .iter()
            .position(|&v| v == value)
            .ok_or_else(|| Error::Protocol(format!("unknown lift-off distance {value}")))?;
        self.write(cmd::SET_LIFT_OFF, &[index as u8])?;
        sleep(Duration::from_millis(30));
        let read_back = self.read(cmd::LIFT_OFF, &[])?[0] as usize;
        p::LOD_VALUES
            .get(read_back)
            .copied()
            .ok_or(Error::Timeout(cmd::LIFT_OFF))
    }

    pub fn set_motion_sync(&mut self, enabled: bool) -> Result<bool> {
        self.write(cmd::SET_MOTION_SYNC, &[enabled as u8])?;
        sleep(Duration::from_millis(30));
        Ok(self.read(cmd::MOTION_SYNC, &[])?[0] != 0)
    }

    /// Verified on hardware: the TEN receiver does not answer command 48.
    pub fn supports_brightness(&mut self) -> Result<bool> {
        self.direct()
    }

    /// Read back a value after a write, retrying like any required read.
    ///
    /// [`Device::read_optional`] makes a single attempt because it exists to
    /// detect commands the hardware does not implement, where one unanswered
    /// read is the answer. The receiver needs about three round trips to reply
    /// after a lighting write, so a single attempt reports every successful
    /// write as unconfirmed.
    fn confirmed(&self, command: u8) -> Result<Vec<u8>> {
        self.read(command, &[])
    }

    fn confirmed_light_mode(&self) -> Result<&'static str> {
        if self.confirmed(cmd::LIGHTING_STATE)?[0] == 0 {
            return Ok("Off");
        }
        const MODE_MAP: [&str; 4] = ["", "Static", "Cycling", "Wave"];
        let mode = self.confirmed(cmd::LIGHTING_MODE)?;
        Ok(MODE_MAP
            .get(mode[0] as usize)
            .copied()
            .filter(|name| !name.is_empty())
            .unwrap_or("Static"))
    }

    pub fn set_brightness(&mut self, percent: u8) -> Result<u8> {
        if !self.supports_brightness()? {
            return Err(Error::Unsupported(
                "This receiver has no brightness control -- the TEN answers \
                 lighting state, mode, colour and speed, but not brightness."
                    .into(),
            ));
        }
        self.write(cmd::SET_LIGHTING_BRIGHTNESS, &[percent / 25])?;
        sleep(Duration::from_millis(30));
        Ok(self.confirmed(cmd::LIGHTING_BRIGHTNESS)?[0].saturating_mul(25))
    }

    pub fn set_light_mode(&mut self, mode: &str) -> Result<&'static str> {
        if mode == "Off" {
            self.write(cmd::SET_LIGHTING_STATE, &[0])?;
        } else {
            let index = ["Static", "Cycling", "Wave"]
                .iter()
                .position(|&m| m == mode)
                .ok_or_else(|| Error::Protocol(format!("unknown light mode {mode}")))?;
            self.write(cmd::SET_LIGHTING_STATE, &[1])?;
            self.write(cmd::SET_LIGHTING_MODE, &[index as u8 + 1])?;
        }
        sleep(Duration::from_millis(30));
        self.confirmed_light_mode()
    }

    pub fn set_color(&mut self, hex_colour: &str) -> Result<String> {
        self.write(cmd::SET_LIGHTING_COLOR, &p::hex_to_rgb(hex_colour)?)?;
        sleep(Duration::from_millis(30));
        let value = self.confirmed(cmd::LIGHTING_COLOR)?;
        Ok(p::rgb_to_hex(value[0], value[1], value[2]))
    }
}

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

    #[test]
    fn uevent_parsing_survives_junk_lines() {
        let fields = vec![("HID_ID".to_string(), "0003:0000093A:0000EB01".to_string())];
        assert_eq!(lookup(&fields, "HID_ID"), Some("0003:0000093A:0000EB01"));
        assert_eq!(lookup(&fields, "HID_NAME"), None);
    }
}
