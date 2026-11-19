use anyhow::{Context, Result, ensure};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

type Source = (&'static str, &'static str);

const WIN64: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";
const LINUX64: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz";
const LINUX_ARM64: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linuxarm64-gpl.tar.xz";
const MACOS_FFMPEG: &str = "https://evermeet.cx/ffmpeg/getrelease/ffmpeg/zip";
const MACOS_FFPROBE: &str = "https://evermeet.cx/ffmpeg/getrelease/ffprobe/zip";

const HARDWARE_ENCODERS: [&str; 5] = [
    "h264_amf",
    "h264_nvenc",
    "h264_qsv",
    "h264_vaapi",
    "h264_videotoolbox",
];

pub fn ffmpeg_works(path: &Path) -> bool {
    Command::new(path)
        .args(["-hide_banner", "-version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn has_hardware_encoders(path: &Path) -> bool {
    Command::new(path)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|out| {
            let list = String::from_utf8_lossy(&out.stdout);
            HARDWARE_ENCODERS
                .iter()
                .any(|name| list.split_whitespace().any(|word| word == *name))
        })
        .unwrap_or(false)
}

// Download a full FFmpeg build (ffmpeg + ffprobe, with every hardware H.264
// encoder compiled in) into a per-user cache directory. Reuses a previous
// download when present.
pub fn install() -> Result<PathBuf> {
    let cache = cache_dir();
    let ffmpeg = cache.join(executable("ffmpeg"));
    if fs::metadata(&ffmpeg).is_ok() && ffmpeg_works(&ffmpeg) {
        return Ok(ffmpeg);
    }
    eprintln!(
        "FFmpeg was not found or lacks GPU encoders; downloading a full build to {}.",
        cache.display()
    );
    fs::create_dir_all(&cache).with_context(|| {
        format!(
            "could not create the FFmpeg cache directory {}",
            cache.display()
        )
    })?;
    let staging = tempfile::TempDir::new_in(&cache)
        .context("could not create a download directory in the FFmpeg cache")?;
    let extract = staging.path().join("extract");
    fs::create_dir(&extract)?;
    for (url, name) in sources() {
        let archive = staging.path().join(name);
        fetch(url, &archive)?;
        unpack(&archive, &extract)?;
        let _ = fs::remove_file(&archive);
    }
    publish(&find_binary(&extract, &executable("ffmpeg"))?, &ffmpeg)?;
    publish(
        &find_binary(&extract, &executable("ffprobe"))?,
        &cache.join(executable("ffprobe")),
    )?;
    ensure!(
        ffmpeg_works(&ffmpeg),
        "the downloaded FFmpeg binary does not run"
    );
    eprintln!("Installed FFmpeg in {}.", cache.display());
    Ok(ffmpeg)
}

fn fetch(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args(["-fL", "--connect-timeout", "15", "--retry", "3", "-o"])
        .arg(destination)
        .arg(url)
        .status()
        .context(
            "downloading FFmpeg needs curl; install curl or install FFmpeg manually (see README)",
        )?;
    ensure!(status.success(), "could not download {url}");
    Ok(())
}

fn unpack(archive: &Path, into: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .context("unpacking FFmpeg needs tar; unpack the build manually (see README)")?;
    ensure!(status.success(), "could not unpack {}", archive.display());
    Ok(())
}

fn publish(source: &Path, destination: &Path) -> Result<()> {
    let _ = fs::remove_file(destination);
    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }
    fs::copy(source, destination)
        .with_context(|| format!("could not install {}", destination.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn find_binary(root: &Path, name: &str) -> Result<PathBuf> {
    let mut found = None;
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("could not read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if pending.len() < 256 {
                    pending.push(path);
                }
            } else if found.is_none() && entry.file_name() == name {
                found = Some(path);
            }
        }
    }
    found.with_context(|| format!("no {name} binary found in the downloaded archives"))
}

fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FILE2VIDEO_CACHE") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(base).join("file2video");
        }
    } else {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
        if let Some(base) = base {
            return base.join("file2video");
        }
    }
    std::env::temp_dir().join("file2video")
}

fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn sources() -> &'static [Source] {
    if cfg!(windows) {
        &[(WIN64, "ffmpeg-win64-gpl.zip")]
    } else if cfg!(target_os = "macos") {
        &[(MACOS_FFMPEG, "ffmpeg.zip"), (MACOS_FFPROBE, "ffprobe.zip")]
    } else if cfg!(target_arch = "aarch64") {
        &[(LINUX_ARM64, "ffmpeg-linux-arm64-gpl.tar.xz")]
    } else {
        &[(LINUX64, "ffmpeg-linux64-gpl.tar.xz")]
    }
}

#[cfg(test)]
mod tests {
    use super::{cache_dir, executable, sources};
    use std::path::PathBuf;

    #[test]
    fn sources_are_well_formed_and_unique() {
        let list = sources();
        assert!(!list.is_empty());
        for (url, name) in list {
            assert!(url.starts_with("https://"), "{url}");
            assert!(url.contains("ffmpeg"), "{url}");
            assert!(!name.is_empty() && !name.starts_with('.'), "{name}");
            assert!(!list.iter().any(|(u, n)| n == name && *u != *url));
        }
    }

    #[test]
    fn cache_dir_honors_override() {
        unsafe {
            std::env::set_var("FILE2VIDEO_CACHE", "/tmp/file2video-test-cache");
        }
        assert_eq!(cache_dir(), PathBuf::from("/tmp/file2video-test-cache"));
        unsafe {
            std::env::remove_var("FILE2VIDEO_CACHE");
        }
    }

    #[test]
    fn executable_names_match_platform() {
        #[allow(unexpected_cfgs)]
        let uses_exe = cfg!(windows);
        let name = executable("ffmpeg");
        assert_eq!(name.ends_with(".exe"), uses_exe);
    }
}
