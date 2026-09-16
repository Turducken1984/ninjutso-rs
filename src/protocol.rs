//! Ninjutso HID protocol (Sora V3 / TEN family, "current" generation).
//!
//! Packet layout confirmed against the TEN receiver's own report descriptor:
//!
//! ```text
//! 06 01 ff  09 01  a1 01  85 06  ... 95 0f ... b1 02
//! vendor page FF01, report ID 6, 15-byte Feature report
//! ```
//!
//! Command numbering follows the NinjaForce web panel.

use crate::Error;

pub const REPORT_ID: u8 = 6;
pub const PAYLOAD_LEN: usize = 15;
pub const CONTROL_REPORT_ID: u8 = 3;
pub const CONTROL_PAYLOAD_LEN: usize = 63;

pub const VENDOR_ID: u16 = 0x093A;
pub const TEN_IDS: [u16; 2] = [0xE020, 0xEA01];
pub const TEN_RECEIVER_IDS: [u16; 1] = [0xEB01];
pub const SORA_V3_IDS: [u16; 2] = [0xE010, 0xEB02];

/// Command numbers. Getters and their matching setters are paired by name.
pub mod cmd {
    pub const BATTERY_CHARGING: u8 = 17;
    pub const BATTERY_PERCENT: u8 = 18;
    pub const ONLINE: u8 = 19;
    pub const FIRMWARE: u8 = 21;
    pub const PROFILE: u8 = 16;
    pub const ACTIVE_DPI_STAGE: u8 = 28;
    pub const DPI_STAGE_COUNT: u8 = 30;
    pub const DPI: u8 = 4;
    pub const POLLING_RATE: u8 = 6;
    pub const LIFT_OFF: u8 = 8;
    pub const ANGLE_TUNING: u8 = 10;
    pub const MOTION_SYNC: u8 = 14;
    pub const SYSTEM_MODE: u8 = 12;
    pub const HYPER_CLICK: u8 = 23;
    pub const SLAM_CLICK: u8 = 42;
    pub const OPTICAL_ENGINE: u8 = 50;
    pub const SLEEP_MINUTES: u8 = 25;
    pub const LIGHTING_MODE: u8 = 32;
    pub const LIGHTING_COLOR: u8 = 34;
    pub const LIGHTING_STATE: u8 = 38;
    pub const LIGHTING_SPEED: u8 = 40;
    pub const LIGHTING_BRIGHTNESS: u8 = 48;
    pub const PAIRED_PRODUCT_ID: u8 = 167;

    pub const SET_DPI: u8 = 3;
    pub const SET_ACTIVE_DPI_STAGE: u8 = 27;
    pub const SET_DPI_STAGE_COUNT: u8 = 29;
    pub const SET_POLLING_RATE: u8 = 5;
    pub const SET_LIFT_OFF: u8 = 7;
    pub const SET_ANGLE_TUNING: u8 = 9;
    pub const SET_MOTION_SYNC: u8 = 13;
    pub const SET_SYSTEM_MODE: u8 = 11;
    pub const SET_HYPER_CLICK: u8 = 22;
    pub const SET_SLAM_CLICK: u8 = 41;
    pub const SET_OPTICAL_ENGINE: u8 = 49;
    pub const SET_SLEEP_MINUTES: u8 = 24;
    pub const SET_LIGHTING_MODE: u8 = 31;
    pub const SET_LIGHTING_COLOR: u8 = 33;
    pub const SET_LIGHTING_STATE: u8 = 37;
    pub const SET_LIGHTING_SPEED: u8 = 39;
    pub const SET_LIGHTING_BRIGHTNESS: u8 = 47;
}

/// Commands scoped to the active profile; everything else is sent with profile 0.
const PROFILE_COMMANDS: [u8; 17] = [
    cmd::ACTIVE_DPI_STAGE,
    cmd::DPI_STAGE_COUNT,
    cmd::DPI,
    cmd::POLLING_RATE,
    cmd::LIFT_OFF,
    cmd::ANGLE_TUNING,
    cmd::MOTION_SYNC,
    cmd::SYSTEM_MODE,
    cmd::HYPER_CLICK,
    cmd::SLAM_CLICK,
    cmd::OPTICAL_ENGINE,
    cmd::SLEEP_MINUTES,
    cmd::LIGHTING_MODE,
    cmd::LIGHTING_COLOR,
    cmd::LIGHTING_STATE,
    cmd::LIGHTING_SPEED,
    cmd::LIGHTING_BRIGHTNESS,
];

pub fn is_profile_command(command: u8) -> bool {
    PROFILE_COMMANDS.contains(&command)
}

pub const POLLING_RATES: [u32; 4] = [1000, 2000, 4000, 8000];
pub const LOD_VALUES: [&str; 3] = ["Low", "Medium", "High"];
pub const SYSTEM_MODES: [&str; 3] = ["High Speed", "Competitive", "Ultra"];
pub const LIGHT_MODES: [&str; 4] = ["Off", "Static", "Cycling", "Wave"];
pub const BRIGHTNESS_LEVELS: [u8; 4] = [25, 50, 75, 100];

pub fn build_request(command: u8, profile: u8, args: &[u8]) -> Result<[u8; PAYLOAD_LEN], Error> {
    if args.len() > 8 {
        return Err(Error::Protocol("at most eight argument bytes".into()));
    }
    let mut payload = [0u8; PAYLOAD_LEN];
    payload[0] = command;
    payload[3] = 1;
    payload[5] = args.len() as u8;
    payload[6] = profile;
    payload[7..7 + args.len()].copy_from_slice(args);
    Ok(payload)
}

/// Value bytes of a reply, or `None` if it is not an answer to `command`.
///
/// `buf` is the raw hidraw buffer, so `buf[0]` is the report ID and the payload
/// starts at `buf[1]` -- the same indexing the WebHID DataView uses.
pub fn response_value(buf: &[u8], command: u8) -> Option<&[u8]> {
    if buf.len() < 9 || buf[1] != command {
        return None;
    }
    Some(&buf[8..])
}

pub fn control_payload(resume: bool) -> [u8; CONTROL_PAYLOAD_LEN] {
    let mut payload = [0u8; CONTROL_PAYLOAD_LEN];
    let head: [u8; 5] = if resume { [28, 27, 0, 0, 1] } else { [27, 26, 0, 0, 1] };
    payload[..5].copy_from_slice(&head);
    payload
}

/// What the sensor accepts: multiples of 50 up to [`DPI_MAX`], or -- on a Sora
/// V3, which takes 1-DPI steps -- anything up to [`DPI_MAX_DIRECT`]. The front
/// ends bound their own inputs by these so they cannot offer an impossible DPI.
pub const DPI_MIN: u32 = 50;
pub const DPI_MAX: u32 = 30_000;
pub const DPI_MIN_DIRECT: u32 = 1;
pub const DPI_MAX_DIRECT: u32 = 45_000;

pub fn encode_dpi(dpi: u32, direct: bool) -> Result<[u8; 3], Error> {
    let (step, low, high) = if direct {
        (1, DPI_MIN_DIRECT, DPI_MAX_DIRECT)
    } else {
        (50, DPI_MIN, DPI_MAX)
    };
    if !(low..=high).contains(&dpi) || dpi % step != 0 {
        return Err(Error::Protocol(if direct {
            format!("DPI must be {low}-{high}")
        } else {
            format!("DPI must be {low}-{high} in steps of 50")
        }));
    }
    let enc = if direct { dpi } else { dpi / 50 - 1 };
    Ok([(enc & 0xFF) as u8, ((enc >> 8) & 0xFF) as u8, 0])
}

pub fn decode_dpi(low: u8, high: u8, tenth: u8, direct: bool) -> f64 {
    let enc = ((high as u32) << 8) | low as u32;
    if direct {
        enc as f64 + tenth as f64 / 10.0
    } else {
        ((enc + 1) * 50) as f64
    }
}

pub fn encode_polling(rate: u32) -> Result<u8, Error> {
    POLLING_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|index| index as u8 + 1)
        .ok_or_else(|| Error::Protocol(format!("unsupported report rate {rate}")))
}

pub fn decode_polling(code: u8) -> u32 {
    POLLING_RATES
        .get(code.wrapping_sub(1) as usize)
        .copied()
        .unwrap_or(1000)
}

pub fn decode_firmware(value: &[u8]) -> String {
    value
        .iter()
        .take(3)
        .rev()
        .map(|b| format!("{b:02X}"))
        .collect()
}

pub fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

pub fn hex_to_rgb(value: &str) -> Result<[u8; 3], Error> {
    let v = value.strip_prefix('#').unwrap_or(value);
    if v.len() != 6 {
        return Err(Error::Protocol("colour must be six hex digits".into()));
    }
    let mut out = [0u8; 3];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&v[index * 2..index * 2 + 2], 16)
            .map_err(|_| Error::Protocol("colour must be six hex digits".into()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_header_matches_the_web_panel() {
        let request = build_request(cmd::DPI, 1, &[2]).unwrap();
        assert_eq!(&request[..8], &[cmd::DPI, 0, 0, 1, 0, 1, 1, 2]);
        assert_eq!(request.len(), PAYLOAD_LEN);
    }

    #[test]
    fn request_rejects_more_than_eight_arguments() {
        assert!(build_request(cmd::DPI, 0, &[0; 9]).is_err());
    }

    #[test]
    fn response_is_matched_by_command() {
        let mut buf = [0u8; 16];
        buf[0] = REPORT_ID;
        buf[1] = cmd::BATTERY_PERCENT;
        buf[8] = 82;
        assert_eq!(response_value(&buf, cmd::BATTERY_PERCENT).unwrap()[0], 82);
        assert!(response_value(&buf, cmd::DPI).is_none());
        assert!(response_value(&buf[..4], cmd::BATTERY_PERCENT).is_none());
    }

    #[test]
    fn dpi_round_trips_in_both_encodings() {
        let [low, high, tenth] = encode_dpi(1600, false).unwrap();
        assert_eq!(decode_dpi(low, high, tenth, false), 1600.0);
        let [low, high, tenth] = encode_dpi(1601, true).unwrap();
        assert_eq!(decode_dpi(low, high, tenth, true), 1601.0);
    }

    #[test]
    fn dpi_bounds_and_step_are_enforced() {
        assert!(encode_dpi(1625, false).is_err()); // not a multiple of 50
        assert!(encode_dpi(30_050, false).is_err()); // above the ceiling
        assert!(encode_dpi(1625, true).is_ok()); // Sora V3 takes 1-DPI steps
    }

    #[test]
    fn polling_rate_round_trips_and_falls_back() {
        for rate in POLLING_RATES {
            assert_eq!(decode_polling(encode_polling(rate).unwrap()), rate);
        }
        assert_eq!(decode_polling(0), 1000); // out of range -> documented default
        assert_eq!(decode_polling(9), 1000);
        assert!(encode_polling(500).is_err());
    }

    #[test]
    fn firmware_bytes_are_reversed_into_a_version() {
        assert_eq!(decode_firmware(&[0x13, 0x16, 0xAE, 0x00]), "AE1613");
    }

    #[test]
    fn colours_round_trip() {
        assert_eq!(hex_to_rgb("#36ad6a").unwrap(), [0x36, 0xAD, 0x6A]);
        assert_eq!(rgb_to_hex(0x36, 0xAD, 0x6A), "#36ad6a");
        assert!(hex_to_rgb("#36ad6").is_err());
    }
}
