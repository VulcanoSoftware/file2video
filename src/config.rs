//! Optional user configuration file. Auto-loaded from `file2video.conf` in
//! the working directory, or from an explicit `--config <PATH>`. Simple
//! `key = value` lines; `#` or `;` start comments. Every key is optional and
//! explicit command-line options always win over values from this file.

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use std::path::Path;

use crate::calibrate::Target;
use crate::format::MAX_KDF;
use crate::video::Gpu;

pub const PRESETS: [&str; 9] = [
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

pub const FILE_NAME: &str = "file2video.conf";

#[derive(Clone, Copy, ValueEnum)]
pub enum Profile {
    Light,
    Medium,
    High,
}

#[derive(Default)]
pub struct Config {
    pub width: Option<usize>,
    pub height: Option<usize>,
    pub block_size: Option<usize>,
    pub fps: Option<u8>,
    pub compress: Option<Profile>,
    pub crf: Option<u8>,
    pub preset: Option<String>,
    pub ecc_symbols: Option<u8>,
    pub pbkdf2_iterations: Option<u32>,
    pub threads: Option<usize>,
    pub chroma: Option<bool>,
    pub intra: Option<bool>,
    pub gpu: Option<Gpu>,
    pub optimise: Option<Target>,
    pub stealth_intensity: Option<usize>,
    pub stealth_block: Option<usize>,
    pub stealth_dense: Option<bool>,
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "on" | "yes" | "true" | "1" => Some(true),
        "off" | "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_number(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("not a valid number: '{value}'"))
}

fn contains(value: u64, range: std::ops::RangeInclusive<u64>) -> Result<()> {
    if range.contains(&value) {
        return Ok(());
    }
    bail!("expected a value in {}..={}", range.start(), range.end())
}

impl Config {
    pub fn from_text(text: &str, name: &str) -> Result<Config> {
        let mut config = Config::default();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split(['#', ';']).next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let number = index + 1;
            let (key, value) = line.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("{name}:{number}: expected 'key = value' (found '{raw}')")
            })?;
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            let which = || format!("{name}:{number}: {key} = {value}");
            match key.as_str() {
                "width" => config.width = Some(parse_number(value)? as usize),
                "height" => config.height = Some(parse_number(value)? as usize),
                "block_size" => config.block_size = Some(parse_number(value)? as usize),
                "fps" => {
                    let parsed = parse_number(value)?;
                    contains(parsed, 1..=60).with_context(|| which().clone())?;
                    config.fps = Some(u8::try_from(parsed).with_context(|| which().clone())?);
                }
                "crf" => {
                    let parsed = parse_number(value)?;
                    contains(parsed, 0..=51).with_context(|| which().clone())?;
                    config.crf = Some(u8::try_from(parsed).with_context(|| which().clone())?);
                }
                "ecc_symbols" => {
                    let parsed = parse_number(value)?;
                    contains(parsed, 1..=127).with_context(|| which().clone())?;
                    config.ecc_symbols =
                        Some(u8::try_from(parsed).with_context(|| which().clone())?);
                }
                "pbkdf2_iterations" => {
                    let parsed = parse_number(value)?;
                    contains(parsed, 1..=MAX_KDF as u64).with_context(|| which().clone())?;
                    config.pbkdf2_iterations =
                        Some(u32::try_from(parsed).with_context(|| which().clone())?);
                }
                "threads" => config.threads = Some(parse_number(value)? as usize),
                "preset" => {
                    if !PRESETS.contains(&value) {
                        bail!("{}: preset must be one of {}", which(), PRESETS.join(", "));
                    }
                    config.preset = Some(value.to_owned());
                }
                "compress" => {
                    config.compress = Some(match value {
                        "light" => Profile::Light,
                        "medium" => Profile::Medium,
                        "high" => Profile::High,
                        _ => bail!("{}: compress must be light, medium or high", which()),
                    });
                }
                "chroma" => {
                    config.chroma = Some(parse_bool(value).with_context(|| which().clone())?);
                }
                "stealth_intensity" => {
                    let parsed = parse_number(value)?;
                    contains(parsed, 5..=50).with_context(|| which().clone())?;
                    config.stealth_intensity = Some(parsed as usize);
                }
                "stealth_block" => {
                    let parsed = parse_number(value)?;
                    contains(parsed, 2..=16).with_context(|| which().clone())?;
                    ensure!(
                        parsed % 2 == 0,
                        "{}: stealth_block must be an even number of pixels",
                        which()
                    );
                    config.stealth_block = Some(parsed as usize);
                }
                "stealth_dense" => {
                    config.stealth_dense =
                        Some(parse_bool(value).with_context(|| which().clone())?);
                }
                "intra" => {
                    config.intra = Some(parse_bool(value).with_context(|| which().clone())?);
                }
                "gpu" => {
                    config.gpu = Some(Gpu::from_name(value).with_context(|| {
                        anyhow::anyhow!(
                            "{}: gpu must be auto, off, amf, nvenc, qsv, vaapi or video-toolbox",
                            which()
                        )
                    })?);
                }
                "optimise" => {
                    config.optimise = Some(Target::from_name(value).with_context(|| {
                        anyhow::anyhow!(
                            "{}: optimise must be off, smallest, fastest or balanced",
                            which()
                        )
                    })?);
                }
                _ => bail!("{name}:{number}: unknown configuration key '{key}'"),
            }
        }
        Ok(config)
    }

    /// Loads the explicit `--config` path (missing files are an error) or, by
    /// default, `file2video.conf` from the working directory when it exists.
    pub fn load(explicit: Option<&Path>) -> Result<Config> {
        Self::load_at(explicit, &std::env::current_dir()?)
    }

    /// Same lookup against an explicit directory, so tests need to touch the
    /// real working directory.
    pub fn load_at(explicit: Option<&Path>, cwd: &Path) -> Result<Config> {
        let (path, required) = match explicit {
            Some(path) => (path.to_owned(), true),
            None => (cwd.join(FILE_NAME), false),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if required {
                    return Err(error).with_context(|| {
                        format!("could not read the configuration file {}", path.display())
                    });
                }
                return Ok(Config::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        Config::from_text(&text, &path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Target};
    use crate::video::Gpu;
    use std::path::Path;

    #[test]
    fn parses_a_complete_file() {
        let text = "
            # comment
            width = 1280
            height = 720
            block_size = 10
            fps = 24
            compress = medium
            crf = 6
            preset = slow
            ecc_symbols = 64
            pbkdf2_iterations = 200000
            threads = 4
            chroma = on
            intra = yes
            gpu = amf
            optimise = balanced
            stealth_intensity = 40
            stealth_dense = on
            stealth_block = 6
        ";
        let config = Config::from_text(text, "file2video.conf").unwrap();
        assert_eq!(config.width, Some(1280));
        assert_eq!(config.height, Some(720));
        assert_eq!(config.block_size, Some(10));
        assert_eq!(config.fps, Some(24));
        assert!(matches!(config.compress, Some(super::Profile::Medium)));
        assert_eq!(config.crf, Some(6));
        assert_eq!(config.preset.as_deref(), Some("slow"));
        assert_eq!(config.ecc_symbols, Some(64));
        assert_eq!(config.pbkdf2_iterations, Some(200000));
        assert_eq!(config.threads, Some(4));
        assert_eq!(config.chroma, Some(true));
        assert_eq!(config.intra, Some(true));
        assert_eq!(config.gpu, Some(Gpu::Amf));
        assert_eq!(config.optimise, Some(Target::Balanced));
        assert_eq!(config.stealth_intensity, Some(40));
        assert_eq!(config.stealth_dense, Some(true));
        assert_eq!(config.stealth_block, Some(6));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(Config::from_text("colour_mode = yes\n", "f").is_err());
        assert!(Config::from_text("width\n", "f").is_err());
        assert!(Config::from_text("fps = 61\n", "f").is_err());
        assert!(Config::from_text("crf = 99\n", "f").is_err());
        assert!(Config::from_text("chroma = maybe\n", "f").is_err());
        assert!(Config::from_text("gpu = dxva\n", "f").is_err());
        assert!(Config::from_text("optimise = tiny\n", "f").is_err());
        assert!(Config::from_text("preset = warp\n", "f").is_err());
        assert!(Config::from_text("stealth_intensity = 4\n", "f").is_err());
        assert!(Config::from_text("stealth_intensity = 51\n", "f").is_err());
        assert!(Config::from_text("stealth_dense = maybe\n", "f").is_err());
        assert!(Config::from_text("stealth_block = 5\n", "f").is_err());
        assert!(Config::from_text("stealth_block = 18\n", "f").is_err());
        let config = Config::from_text("optimise = off\nchroma = 0\n", "f").unwrap();
        assert_eq!(config.optimise, Some(Target::Off));
        assert_eq!(config.chroma, Some(false));
    }

    #[test]
    fn empty_files_are_valid() {
        let config = Config::from_text("", "f").unwrap();
        assert_eq!(config.width, None);
        assert_eq!(config.optimise, None);
    }

    #[test]
    fn loads_the_working_directory_file_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(super::FILE_NAME), "fps = 60\n").unwrap();
        assert_eq!(Config::load_at(None, dir.path()).unwrap().fps, Some(60));
        let empty = Path::new("/nonexistent-directory");
        assert_eq!(Config::load_at(None, empty).unwrap().fps, None);
    }

    #[test]
    fn explicit_config_paths_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("custom.conf");
        std::fs::write(&file, "width = 640\n").unwrap();
        assert_eq!(
            Config::load_at(Some(&file), dir.path()).unwrap().width,
            Some(640)
        );
        let missing = dir.path().join("missing.conf");
        assert!(Config::load_at(Some(&missing), dir.path()).is_err());
    }
}
