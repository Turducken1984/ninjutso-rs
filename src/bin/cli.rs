//! Terminal front end -- same protocol layer as the GUI.

use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use ninjutso::device::Device;
use ninjutso::firmware::latest_for;
use ninjutso::protocol as p;
use ninjutso::Error;

#[derive(Parser)]
#[command(name = "ninjutso", about = "Configure Ninjutso mice on Linux.", version)]
struct Cli {
    /// hidraw path (default: autodetect)
    #[arg(long, global = true)]
    device: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// show everything
    Status,
    /// report firmware versions and check for updates
    Firmware {
        /// skip the online check
        #[arg(long)]
        offline: bool,
    },
    /// set DPI (active stage unless --stage given)
    Dpi {
        value: u32,
        /// which DPI stage to write (default: the active one)
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=4))]
        stage: Option<u8>,
    },
    /// list all DPI stages
    Stages,
    /// set report rate in Hz
    Rate {
        #[arg(value_parser = parse_rate)]
        value: u32,
    },
    /// set lift-off distance
    Lod {
        #[arg(value_parser = p::LOD_VALUES)]
        value: String,
    },
    /// receiver lighting
    Light(LightArgs),
}

#[derive(Args)]
struct LightArgs {
    #[arg(long, value_parser = p::LIGHT_MODES)]
    mode: Option<String>,
    /// hex colour, e.g. #36ad6a
    #[arg(long)]
    color: Option<String>,
    #[arg(long, value_parser = parse_brightness)]
    brightness: Option<u8>,
}

fn parse_rate(value: &str) -> Result<u32, String> {
    let rate: u32 = value.parse().map_err(|_| format!("invalid number {value}"))?;
    if p::POLLING_RATES.contains(&rate) {
        Ok(rate)
    } else {
        Err(format!(
            "possible values: {}",
            p::POLLING_RATES.map(|r| r.to_string()).join(", ")
        ))
    }
}

fn parse_brightness(value: &str) -> Result<u8, String> {
    let level: u8 = value.parse().map_err(|_| format!("invalid number {value}"))?;
    if p::BRIGHTNESS_LEVELS.contains(&level) {
        Ok(level)
    } else {
        Err(format!(
            "possible values: {}",
            p::BRIGHTNESS_LEVELS.map(|b| b.to_string()).join(", ")
        ))
    }
}

/// Print a before/after line, or note that the device never confirmed.
fn report<T: std::fmt::Display>(label: &str, before: impl std::fmt::Display, after: Option<T>, unit: &str) -> u8 {
    match after {
        None => {
            println!("  {label}: device did not confirm");
            1
        }
        Some(after) => {
            println!("  {label} {before}{unit} → {after}{unit}  ✓ confirmed by device");
            0
        }
    }
}

fn cmd_status(device: &mut Device) -> Result<u8, Error> {
    let s = device.status()?;
    let name = s.name.replace("Ninjutso Inc. ", "");
    println!(
        "  {name}  ({}, {:04x}:{:04x})",
        s.path.display(),
        p::VENDOR_ID,
        s.product_id
    );
    let charge = match s.charging {
        None => "charging status unavailable".to_string(),
        Some(true) => "charging".to_string(),
        Some(false) => "not charging".to_string(),
    };
    match s.battery {
        Some(percent) => println!("  Battery    {percent}%   {charge}"),
        None => println!("  Battery    no reading   {charge}"),
    }
    let stages = s
        .dpi_stages
        .iter()
        .map(|v| (*v as i64).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "  DPI        {}  stage {} of {}   [{stages}]",
        s.dpi.unwrap_or(0.0) as i64,
        s.dpi_stage + 1,
        s.dpi_stages.len()
    );
    println!("  Report     {} Hz", s.polling_rate);
    println!("  Lift-off   {}", s.lift_off.unwrap_or("—"));
    println!("  Motion     {}", if s.motion_sync { "on" } else { "off" });
    if let Some(mode) = s.system_mode {
        println!("  Mode       {mode}");
    }
    if let Some(light) = &s.lighting {
        let bright = light
            .brightness
            .map_or_else(|| "—".to_string(), |b| format!("{b}%"));
        println!(
            "  Light      {}  {}  {bright}",
            light.mode,
            light.color.as_deref().unwrap_or("")
        );
    }
    if !s.firmware.is_empty() {
        let parts = s
            .firmware
            .iter()
            .map(|(part, version)| format!("{part} {version}"))
            .collect::<Vec<_>>()
            .join(" · ");
        println!("  Firmware   {parts}");
    }
    Ok(0)
}

fn cmd_dpi(device: &mut Device, value: u32, stage: Option<u8>) -> Result<u8, Error> {
    let status = device.status()?;
    let active = status.dpi_stage;
    let index = stage.map_or(active, |s| s as usize - 1);
    if index >= status.dpi_stages.len() {
        eprintln!(
            "error: this mouse has {} DPI stages",
            status.dpi_stages.len()
        );
        return Ok(2);
    }
    let before = status.dpi_stages[index] as i64;
    let after = device.set_dpi(value, Some(index))? as i64;
    let label = format!(
        "Stage {} DPI{}",
        index + 1,
        if index == active { "" } else { " (inactive)" }
    );
    Ok(report(&label, before, Some(after), ""))
}

fn cmd_stages(device: &mut Device) -> Result<u8, Error> {
    let status = device.status()?;
    for (index, value) in status.dpi_stages.iter().enumerate() {
        let mark = if index == status.dpi_stage { "*" } else { " " };
        println!("  {mark} stage {}   {}", index + 1, *value as i64);
    }
    println!("\n  * = active stage");
    Ok(0)
}

fn cmd_light(device: &mut Device, args: &LightArgs) -> Result<u8, Error> {
    let light = device.read_lighting()?;
    let mut rc = 0;
    if let Some(mode) = &args.mode {
        let before = light.as_ref().map_or("—", |l| l.mode);
        rc |= report("Light mode", before, Some(device.set_light_mode(mode)?), "");
    }
    if let Some(color) = &args.color {
        let before = light
            .as_ref()
            .and_then(|l| l.color.clone())
            .unwrap_or_else(|| "—".to_string());
        rc |= report("Colour", before, Some(device.set_color(color)?), "");
    }
    if let Some(brightness) = args.brightness {
        let before = light
            .as_ref()
            .and_then(|l| l.brightness)
            .map_or_else(|| "—".to_string(), |b| b.to_string());
        match device.set_brightness(brightness) {
            Ok(after) => rc |= report("Brightness", before, Some(after), "%"),
            Err(err @ Error::Unsupported(_)) => {
                println!("  {err}");
                rc = 1;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(rc)
}

fn cmd_firmware(device: &mut Device, offline: bool) -> Result<u8, Error> {
    let status = device.status()?;
    if status.firmware.is_empty() {
        println!("  no firmware version reported");
        return Ok(1);
    }

    let mut outdated = false;
    let mut unreachable = false;
    for (part, installed) in &status.firmware {
        let pid = match *part {
            "mouse" => status.effective_product_id,
            _ => status.product_id,
        };
        let latest = if offline {
            None
        } else {
            latest_for(pid, Duration::from_secs(6))
        };
        // `part` is "mouse" or "receiver"; title-case it the way Python did.
        let label = format!("{}{}", part[..1].to_uppercase(), &part[1..]);
        match latest {
            None => {
                let note = if offline { "" } else { "  (latest unknown)" };
                unreachable |= !offline;
                println!("  {label:9} {installed}{note}");
            }
            Some(latest) if &latest == installed => {
                println!("  {label:9} {installed}   latest {latest}   up to date");
            }
            Some(latest) => {
                outdated = true;
                println!("  {label:9} {installed}   latest {latest}   UPDATE AVAILABLE");
            }
        }
    }

    if unreachable {
        println!("\n  Could not reach Ninjutso's version service; installed versions shown above.");
    }
    if outdated {
        println!(
            "\n  Updating is Windows-only -- Ninjutso ship firmware as an .exe.\n  \
             This tool never writes firmware."
        );
        return Ok(1);
    }
    println!("\n  This tool never writes firmware; flashing is Windows-only.");
    Ok(0)
}

fn run() -> Result<u8, Error> {
    let cli = Cli::parse();
    let mut device = match &cli.device {
        Some(path) => Device::open_path(path)?,
        None => Device::open()?,
    };
    match &cli.command {
        Command::Status => cmd_status(&mut device),
        Command::Firmware { offline } => cmd_firmware(&mut device, *offline),
        Command::Dpi { value, stage } => cmd_dpi(&mut device, *value, *stage),
        Command::Stages => cmd_stages(&mut device),
        Command::Rate { value } => {
            let before = device.status()?.polling_rate;
            let after = device.set_polling_rate(*value)?;
            Ok(report("Report rate", before, Some(after), " Hz"))
        }
        Command::Lod { value } => {
            let before = device.status()?.lift_off.unwrap_or("—");
            let after = device.set_lift_off(value)?;
            Ok(report("Lift-off", before, Some(after), ""))
        }
        Command::Light(args) => cmd_light(&mut device, args),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) if err.is_access_problem() => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}
