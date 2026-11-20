//! Best-effort system diagnostics printed at startup: CPU, memory, GPU
//! driver, and the FFmpeg build being used. Every probe may silently fail;
//! a missing detail is simply omitted.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::download::HARDWARE_ENCODERS;

fn platform_encoders() -> &'static str {
    if cfg!(target_os = "macos") {
        "h264_videotoolbox"
    } else if cfg!(windows) {
        "h264_amf (AMD), h264_nvenc (NVIDIA), h264_qsv (Intel)"
    } else {
        "h264_vaapi, h264_nvenc (NVIDIA), h264_qsv (Intel)"
    }
}

fn command_output(command: &mut Command) -> Option<String> {
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn powershell(script: &str) -> Command {
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-Command", script]);
    command
}

fn gigabytes(bytes: &str) -> String {
    let value: u64 = bytes.trim().parse().unwrap_or(0);
    format!("{:.1} GB", value as f64 / 1e9)
}

fn cpu() -> Option<String> {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("model name"))
            .map(|value| value.trim_start_matches(['\t', ' ', ':']).trim().to_owned())
    } else if cfg!(target_os = "macos") {
        command_output(
            Command::new("sysctl")
                .arg("-n")
                .arg("machdep.cpu.brand_string"),
        )
        .map(|value| value.trim().to_owned())
    } else if cfg!(windows) {
        let mut command = powershell(
            "Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name",
        );
        command_output(&mut command).map(|value| value.trim().to_owned())
    } else {
        None
    }
}

fn memory() -> Option<String> {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/meminfo")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .map(|value| {
                let kib: u64 = value
                    .trim()
                    .trim_end_matches(" kB")
                    .trim()
                    .parse()
                    .unwrap_or(0);
                gigabytes(&(kib * 1024).to_string())
            })
    } else if cfg!(target_os = "macos") {
        command_output(Command::new("sysctl").arg("-n").arg("hw.memsize"))
            .map(|value| gigabytes(value.trim()))
    } else if cfg!(windows) {
        let mut command = powershell("(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory");
        command_output(&mut command).map(|value| gigabytes(value.trim()))
    } else {
        None
    }
}

/// Detected GPUs as reported by the OS, empty when they cannot be listed.
pub fn gpu_names() -> Vec<String> {
    gpu().unwrap_or_default()
}

fn cpu_name() -> Option<String> {
    cpu().filter(|name| !name.trim().is_empty())
}

/// The coreset of the specs recorded in the calibration file. A change in any
/// component (hardware, driver, core count, or FFmpeg build) changes the
/// fingerprint, so a saved calibration is automatically considered stale.
pub fn fingerprint(ffmpeg: &Path) -> String {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut parts = vec![
        std::env::consts::OS.to_owned(),
        std::env::consts::ARCH.to_owned(),
        cpu_name().unwrap_or_else(|| "unknown cpu".to_owned()),
        cores.to_string(),
    ];
    let gpus = gpu_names();
    parts.push(if gpus.is_empty() {
        "gpu unknown".to_owned()
    } else {
        format!("gpus: {}", gpus.join("; "))
    });
    if let Some(summary) = ffmpeg_summary(ffmpeg) {
        parts.push(summary);
    } else {
        parts.push("ffmpeg unknown".to_owned());
    }
    parts.join(" | ")
}

fn gpu() -> Option<Vec<String>> {
    if cfg!(target_os = "linux") {
        command_output(Command::new("lspci").args(["-nn", "-d", "::03xx"])).map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
    } else if cfg!(target_os = "macos") {
        command_output(Command::new("system_profiler").args([
            "SPDisplaysDataType",
            "-detailLevel",
            "mini",
        ]))
        .map(|text| {
            text.lines()
                .filter_map(|line| line.trim().strip_prefix("Chipset Model:"))
                .map(|value| value.trim().to_owned())
                .collect()
        })
    } else if cfg!(windows) {
        let mut command = powershell(
            "Get-CimInstance Win32_VideoController | ForEach-Object { \"{0} (driver {1})\" -f $_.Name, $_.DriverVersion }",
        );
        command_output(&mut command).map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
    } else {
        None
    }
}

fn ffmpeg_summary(ffmpeg: &Path) -> Option<String> {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-version"]);
    let version = command_output(&mut command)?
        .lines()
        .next()?
        .trim()
        .to_owned();
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-encoders"]);
    let list = command_output(&mut command)?;
    let found: Vec<String> = HARDWARE_ENCODERS
        .iter()
        .filter(|name| list.split_whitespace().any(|word| word == **name))
        .map(ToString::to_string)
        .collect();
    Some(format!("{version} | hardware encoders: {found:?}"))
}

/// Print one diagnostics block to stderr. Values that cannot be determined
/// are skipped rather than printed as empty.
pub fn print(ffmpeg: &Path) {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "{} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    if let Some(cpu) = cpu() {
        parts.push(format!("{cpu} ({cores} threads)"));
    } else {
        parts.push(format!("{cores} threads"));
    }
    if let Some(memory) = memory() {
        parts.push(memory);
    }
    eprintln!("System: {}", parts.join(" | "));
    match gpu() {
        Some(gpus) if !gpus.is_empty() => {
            eprintln!("GPU: {}", gpus.join("; "));
        }
        _ => eprintln!("GPU: unknown ({})", platform_encoders()),
    }
    match ffmpeg_summary(ffmpeg) {
        Some(summary) => eprintln!("FFmpeg: {summary}"),
        None => eprintln!("FFmpeg: {} (could not be queried)", ffmpeg.display()),
    }
}
