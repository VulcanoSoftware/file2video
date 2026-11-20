//! Hardware calibration: measurements produced by `benchmark`, saved next to
//! the program, and reused by `encode` with an optimise target so the best
//! backend and frame mode for this machine is picked without benchmarking
//! again. The file records a hardware and FFmpeg fingerprint, so a driver
//! upgrade or a hardware change automatically triggers a fresh calibration.

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::path::Path;

/// What to optimise for when encoding. `Off` keeps the explicitly configured
/// backend and frame mode, i.e. the pre-optimisation behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Target {
    Off,
    Smallest,
    Fastest,
    Balanced,
}

impl Target {
    pub fn from_name(name: &str) -> Option<Target> {
        Some(match name {
            "off" | "none" => Target::Off,
            "smallest" | "size" => Target::Smallest,
            "fastest" | "speed" => Target::Fastest,
            "balanced" | "balance" => Target::Balanced,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Target::Off => "off",
            Target::Smallest => "smallest",
            Target::Fastest => "fastest",
            Target::Balanced => "balanced",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Measurement {
    /// Short backend name: "cpu" or a GPU encoder name such as "amf".
    pub backend: String,
    pub intra: bool,
    pub chroma: bool,
    pub frames: u64,
    pub payload: u64,
    pub encode_s: f64,
    pub decode_s: f64,
    pub mp4: u64,
}

impl Measurement {
    fn seconds_per_byte(&self) -> f64 {
        (self.encode_s + self.decode_s) / self.payload.max(1) as f64
    }

    fn size_ratio(&self) -> f64 {
        self.mp4 as f64 / self.payload.max(1) as f64
    }

    fn line(&self) -> String {
        format!(
            "measured = {}|{}|{}|{}|{}|{:.3}|{:.3}|{}",
            self.backend,
            u8::from(self.intra),
            u8::from(self.chroma),
            self.frames,
            self.payload,
            self.encode_s,
            self.decode_s,
            self.mp4
        )
    }

    fn score(&self, target: Target, time: &Span, size: &Span) -> f64 {
        match target {
            Target::Off => 0.0,
            Target::Smallest => self.size_ratio(),
            Target::Fastest => self.seconds_per_byte(),
            Target::Balanced => {
                (self.seconds_per_byte() - time.min) / time.width
                    + (self.size_ratio() - size.min) / size.width
            }
        }
    }
}

/// Range of a metric across the candidate rows, for min-max normalisation.
struct Span {
    min: f64,
    width: f64,
}

impl Span {
    fn new(values: impl Iterator<Item = f64>) -> Span {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for value in values {
            min = min.min(value);
            max = max.max(value);
        }
        if min.is_finite() {
            Span {
                min,
                width: (max - min).max(f64::EPSILON * 8.0),
            }
        } else {
            Span {
                min: 0.0,
                width: 0.0,
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Failure {
    pub backend: String,
    pub intra: bool,
    pub chroma: bool,
}

#[derive(Debug)]
pub struct Calibration {
    pub fingerprint: String,
    pub saved: u64,
    pub measurements: Vec<Measurement>,
    pub failures: Vec<Failure>,
}

pub const FILE_NAME: &str = "file2video-calibration.conf";

const MARKER: &str = "file2video-calibration v1";

fn saved_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn flag(field: &str) -> Result<bool> {
    match field.trim() {
        "1" => Ok(true),
        "0" => Ok(false),
        other => bail!("expected 0 or 1, found '{other}'"),
    }
}

fn parse_measurement(value: &str) -> Result<Measurement> {
    let fields: Vec<&str> = value.split('|').collect();
    if fields.len() != 8 {
        bail!("expected 8 |-separated fields");
    }
    Ok(Measurement {
        backend: fields[0].trim().to_owned(),
        intra: flag(fields[1])?,
        chroma: flag(fields[2])?,
        frames: fields[3].trim().parse()?,
        payload: fields[4].trim().parse()?,
        encode_s: fields[5].trim().parse()?,
        decode_s: fields[6].trim().parse()?,
        mp4: fields[7].trim().parse()?,
    })
}

fn parse_failure(value: &str) -> Result<Failure> {
    let fields: Vec<&str> = value.split('|').collect();
    if fields.len() != 3 {
        bail!("expected 3 |-separated fields");
    }
    Ok(Failure {
        backend: fields[0].trim().to_owned(),
        intra: flag(fields[1])?,
        chroma: flag(fields[2])?,
    })
}

impl Calibration {
    pub fn new(fingerprint: String) -> Calibration {
        Calibration {
            fingerprint,
            saved: saved_now(),
            measurements: Vec::new(),
            failures: Vec::new(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = String::new();
        text.push_str(MARKER);
        text.push_str("\nfingerprint = ");
        text.push_str(&self.fingerprint);
        text.push_str("\nsaved = ");
        text.push_str(&self.saved.to_string());
        text.push('\n');
        for failed in &self.failures {
            text.push_str(&format!(
                "failed = {}|{}|{}\n",
                failed.backend,
                u8::from(failed.intra),
                u8::from(failed.chroma)
            ));
        }
        for measurement in &self.measurements {
            text.push_str(&measurement.line());
            text.push('\n');
        }
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("could not write {}", path.display()))
    }

    /// Loads the calibration file; `None` when it does not exist yet.
    pub fn load(path: &Path) -> Result<Option<Calibration>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not read the calibration file {}", path.display())
                });
            }
        };
        parse_calibration(&text, path).map(Some)
    }

    pub fn is_fresh(&self, fingerprint: &str) -> bool {
        self.fingerprint == fingerprint
    }

    /// Picks the best measurement for the target among the rows that respect
    /// the explicitly configured backend and frame modes. Rows outside those
    /// constraints never win; `None` means no measured candidate matches.
    pub fn choose(
        &self,
        target: Target,
        backend: Option<&str>,
        intra: Option<bool>,
        chroma: Option<bool>,
    ) -> Option<&Measurement> {
        if target == Target::Off {
            return None;
        }
        let candidates: Vec<&Measurement> = self
            .measurements
            .iter()
            .filter(|row| row.backend == backend.unwrap_or(&row.backend))
            .filter(|row| row.intra == intra.unwrap_or(row.intra))
            .filter(|row| row.chroma == chroma.unwrap_or(row.chroma))
            .collect();
        let time = Span::new(candidates.iter().map(|row| row.seconds_per_byte()));
        let size = Span::new(candidates.iter().map(|row| row.size_ratio()));
        candidates.into_iter().min_by(|a, b| {
            a.score(target, &time, &size)
                .total_cmp(&b.score(target, &time, &size))
        })
    }
}

fn parse_calibration(text: &str, path: &Path) -> Result<Calibration> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some(MARKER) {
        bail!(
            "{} is not a file2video calibration file; delete it and run the benchmark again",
            path.display()
        );
    }
    let mut fingerprint = None;
    let mut saved = 0u64;
    let mut measurements = Vec::new();
    let mut failures = Vec::new();
    for (index, raw) in lines.enumerate() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let number = index + 2;
        let (key, value) = raw.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "{}:{number}: expected 'key = value' (found '{raw}')",
                path.display()
            )
        })?;
        let which = || anyhow::anyhow!("{}:{number}: could not parse '{raw}'", path.display());
        match key.trim() {
            "fingerprint" => fingerprint = Some(value.trim().to_owned()),
            "saved" => saved = value.trim().parse().unwrap_or(0),
            "measured" => measurements.push(parse_measurement(value)?),
            "failed" => failures.push(parse_failure(value).with_context(which)?),
            _ => {}
        }
    }
    let fingerprint = fingerprint.ok_or_else(|| {
        anyhow::anyhow!(
            "{} lacks a fingerprint; delete it and run the benchmark again",
            path.display()
        )
    })?;
    Ok(Calibration {
        fingerprint,
        saved,
        measurements,
        failures,
    })
}

#[cfg(test)]
mod tests {
    use super::{Calibration, Failure, Measurement, Target};

    fn row(backend: &str, intra: bool, chroma: bool, enc: f64, dec: f64, mp4: u64) -> Measurement {
        Measurement {
            backend: backend.to_owned(),
            intra,
            chroma,
            frames: 100,
            payload: 8 * 1024 * 1024,
            encode_s: enc,
            decode_s: dec,
            mp4,
        }
    }

    fn calibration_with(rows: Vec<Measurement>) -> Calibration {
        let mut calibration = Calibration::new("test machine".to_owned());
        calibration.measurements = rows;
        calibration
    }

    #[test]
    fn selects_the_target_optimum() {
        let calibration = calibration_with(vec![
            row("cpu", false, false, 8.0, 2.0, 14_000_000),
            row("cpu", true, false, 4.0, 4.0, 13_000_000),
            row("cpu", false, true, 6.5, 1.8, 14_100_000),
            row("cpu", true, true, 3.6, 3.4, 13_100_000),
            row("amf", false, false, 3.0, 3.0, 28_000_000),
        ]);
        let smallest = calibration
            .choose(Target::Smallest, None, None, None)
            .unwrap();
        assert_eq!(
            (smallest.backend.as_str(), smallest.intra, smallest.chroma),
            ("cpu", true, false)
        );
        let fastest = calibration
            .choose(Target::Fastest, None, None, None)
            .unwrap();
        assert_eq!(
            (fastest.backend.as_str(), fastest.intra, fastest.chroma),
            ("amf", false, false)
        );
        let balanced = calibration
            .choose(Target::Balanced, None, None, None)
            .unwrap();
        assert_eq!(
            (balanced.backend.as_str(), balanced.intra, balanced.chroma),
            ("cpu", true, true)
        );
        let forced = calibration
            .choose(Target::Smallest, Some("amf"), None, None)
            .unwrap();
        assert_eq!(forced.backend, "amf");
        assert!(
            calibration
                .choose(Target::Smallest, Some("nvenc"), None, None)
                .is_none()
        );
        assert!(calibration.choose(Target::Off, None, None, None).is_none());
        let intra_only = calibration
            .choose(Target::Smallest, None, Some(true), None)
            .unwrap();
        assert!(intra_only.intra);
    }

    #[test]
    fn round_trips_the_file_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calibration.conf");
        let mut calibration = Calibration::new("windows x86_64 | test machine".to_owned());
        calibration.measurements = vec![row("cpu", true, true, 3.6, 3.3, 13_100_000)];
        calibration.failures = vec![Failure {
            backend: "amf".to_owned(),
            intra: false,
            chroma: true,
        }];
        calibration.save(&path).unwrap();
        let loaded = Calibration::load(&path).unwrap().unwrap();
        assert_eq!(loaded.fingerprint, "windows x86_64 | test machine");
        assert!(loaded.is_fresh("windows x86_64 | test machine"));
        assert!(!loaded.is_fresh("other machine"));
        assert_eq!(loaded.measurements.len(), 1);
        assert_eq!(loaded.measurements[0].backend, "cpu");
        assert!(loaded.measurements[0].intra);
        assert!(loaded.measurements[0].chroma);
        assert_eq!(loaded.measurements[0].encode_s, 3.6);
        assert_eq!(loaded.measurements[0].mp4, 13_100_000);
        assert_eq!(loaded.failures.len(), 1);
        assert!(loaded.failures[0].chroma);
    }

    #[test]
    fn missing_file_is_none_and_garbage_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Calibration::load(&dir.path().join("absent.conf"))
                .unwrap()
                .is_none()
        );
        let path = dir.path().join("garbage.conf");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        assert!(Calibration::load(&path).is_err());
    }
}
