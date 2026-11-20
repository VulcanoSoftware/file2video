use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_file2video"))
        .current_dir(dir)
        .args(["--threads", "2"])
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn round_trip_without_a_password() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let data: Vec<u8> = (0..70_001u32).map(|i| (i * 41) as u8).collect();
    fs::write(dir.join("open.bin"), &data).unwrap();

    success(cli(
        dir,
        &[
            "encode",
            "open.bin",
            "open.mp4",
            "--width",
            "648",
            "--height",
            "360",
            "--preset",
            "ultrafast",
            "--no-password",
            "--pbkdf2-iterations",
            "1000",
        ],
    ));

    // Decoding does not prompt for (or need) a password.
    success(cli(dir, &["decode", "open.mp4", "restored-open.bin"]));
    assert_eq!(fs::read(dir.join("restored-open.bin")).unwrap(), data);
    // A password passed to a plaintext video is accepted and ignored.
    success(cli(
        dir,
        &["decode", "open.mp4", "restored-2.bin", "--password", "abc"],
    ));
    assert_eq!(fs::read(dir.join("restored-2.bin")).unwrap(), data);

    // Encoded videos must still demand the right password.
    fs::write(dir.join("hidden.bin"), &data).unwrap();
    success(cli(
        dir,
        &[
            "encode",
            "hidden.bin",
            "hidden.mp4",
            "--width",
            "648",
            "--height",
            "360",
            "--preset",
            "ultrafast",
            "--password",
            "word",
            "--pbkdf2-iterations",
            "1000",
        ],
    ));
    let wrong = cli(
        dir,
        &["decode", "hidden.mp4", "bad.bin", "--password", "abc"],
    );
    assert!(!wrong.status.success());
    let right = cli(
        dir,
        &[
            "decode",
            "hidden.mp4",
            "restored-3.bin",
            "--password",
            "word",
        ],
    );
    success(right);
    assert_eq!(fs::read(dir.join("restored-3.bin")).unwrap(), data);
}

#[test]
fn round_trip_non_aligned_frames_and_multiple_crypto_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut seed = 0x12ab_34cdu32;
    let data: Vec<u8> = (0..540_103)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    fs::write(dir.join("a file.bin"), &data).unwrap();
    fs::write(dir.join("password.txt"), "démo\r\n").unwrap();
    fs::create_dir(dir.join("restored")).unwrap();

    success(cli(
        dir,
        &[
            "encode",
            "a file.bin",
            "output.mp4",
            "--width",
            "648",
            "--height",
            "360",
            "--preset",
            "ultrafast",
            "--password-file",
            "password.txt",
            "--pbkdf2-iterations",
            "1000",
        ],
    ));
    let wrong = cli(
        dir,
        &["decode", "output.mp4", "wrong.bin", "--password", "bad"],
    );
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("authentication failed"));
    assert!(!dir.join("wrong.bin").exists());
    success(cli(
        dir,
        &[
            "decode",
            "output.mp4",
            "restored",
            "--password-file",
            "password.txt",
        ],
    ));
    assert_eq!(fs::read(dir.join("restored/a file.bin")).unwrap(), data);

    let existing = cli(
        dir,
        &[
            "decode",
            "output.mp4",
            "a file.bin",
            "--password-file",
            "password.txt",
        ],
    );
    assert!(!existing.status.success());
    assert_eq!(fs::read(dir.join("a file.bin")).unwrap(), data);
    assert!(fs::read_dir(dir).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".file2video-")
    }));
}

#[test]
fn round_trip_chroma_and_all_intra_options() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut seed = 0xfeed_beefu32;
    let data: Vec<u8> = (0..22_237u32)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    fs::write(dir.join("payload.bin"), &data).unwrap();
    fs::write(dir.join("password.txt"), "démo\r\n").unwrap();
    success(cli(
        dir,
        &[
            "encode",
            "payload.bin",
            "chroma.mp4",
            "--chroma",
            "--intra",
            "--password-file",
            "password.txt",
            "--pbkdf2-iterations",
            "1000",
        ],
    ));
    success(cli(
        dir,
        &[
            "decode",
            "chroma.mp4",
            "restored.bin",
            "--password-file",
            "password.txt",
        ],
    ));
    assert_eq!(fs::read(dir.join("restored.bin")).unwrap(), data);
}

#[test]
fn empty_file_and_truncated_video() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    fs::write(dir.join("empty.bin"), []).unwrap();
    let estimate = cli(dir, &["estimate", "empty.bin"]);
    assert!(String::from_utf8_lossy(&estimate.stdout).contains("Frames:           1"));
    success(estimate);
    success(cli(
        dir,
        &[
            "encode",
            "empty.bin",
            "empty.mp4",
            "--password",
            "a",
            "--pbkdf2-iterations",
            "1000",
        ],
    ));
    success(cli(
        dir,
        &["decode", "empty.mp4", "back.bin", "--password", "a"],
    ));
    assert_eq!(fs::metadata(dir.join("back.bin")).unwrap().len(), 0);
    let mut bytes = fs::read(dir.join("empty.mp4")).unwrap();
    bytes.truncate(bytes.len() / 2);
    fs::write(dir.join("short.mp4"), bytes).unwrap();
    assert!(
        !cli(
            dir,
            &["decode", "short.mp4", "short.bin", "--password", "a"]
        )
        .status
        .success()
    );
    assert!(!dir.join("short.bin").exists());
}

#[test]
fn optimise_uses_and_updates_the_calibration_file() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut seed = 0xfeed_beefu32;
    let data: Vec<u8> = (0..300_000)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    fs::write(dir.join("payload.bin"), &data).unwrap();
    fs::write(dir.join("password.txt"), "demo\n").unwrap();
    fs::write(
        dir.join("file2video.conf"),
        "optimise = balanced\nfps = 15\necc_symbols = 16\n",
    )
    .unwrap();

    success(cli(
        dir,
        &[
            "encode",
            "payload.bin",
            "output.mp4",
            "--preset",
            "ultrafast",
            "--password-file",
            "password.txt",
            "--pbkdf2-iterations",
            "1000",
        ],
    ));
    let calibration = fs::read_to_string(dir.join("file2video-calibration.conf")).unwrap();
    assert!(calibration.starts_with("file2video-calibration v1"));
    assert!(calibration.contains("fingerprint = "));
    assert!(calibration.contains("measured = cpu|"));

    success(cli(
        dir,
        &[
            "decode",
            "output.mp4",
            "restored.bin",
            "--password-file",
            "password.txt",
        ],
    ));
    assert_eq!(fs::read(dir.join("restored.bin")).unwrap(), data);

    let second = cli(dir, &["estimate", "payload.bin", "--optimise", "fastest"]);
    let text = String::from_utf8_lossy(&second.stdout).into_owned();
    success(second);
    assert!(
        text.contains("Optimising for fastest would use: cpu"),
        "{text}"
    );
}

#[test]
fn stealth_round_trip_with_looping_background_audio() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut seed = 0x517c_c0deu32;
    let data: Vec<u8> = (0..12_288u32)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    fs::write(dir.join("payload.bin"), &data).unwrap();

    // A config-level optimise target must not block stealth encodes; the
    // tool warns and encodes with the requested settings instead.
    fs::write(dir.join("file2video.conf"), "optimise = smallest\n").unwrap();

    // One second of 240x144 background video with an audio track; the data
    // layer needs far more frames than that, so both streams must loop.
    success(
        Command::new("ffmpeg")
            .current_dir(dir)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=240x144:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-crf",
                "18",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                "dummy.mp4",
            ])
            .output()
            .unwrap(),
    );
    let encoded = cli(
        dir,
        &[
            "encode",
            "payload.bin",
            "stealth.mp4",
            "--stealth",
            "dummy.mp4",
            "--fps",
            "10",
            "--preset",
            "ultrafast",
            "--password",
            "a",
            "--pbkdf2-iterations",
            "1000",
        ],
    );
    success(encoded.clone());
    let stderr = String::from_utf8_lossy(&encoded.stderr);
    assert!(
        stderr.contains("ignoring the optimise target"),
        "expected an optimise warning, got: {stderr}"
    );

    // The explicit flag remains rejected.
    let rejected = cli(
        dir,
        &[
            "encode",
            "payload.bin",
            "stealth-2.mp4",
            "--stealth",
            "dummy.mp4",
            "--optimise",
            "smallest",
            "--password",
            "a",
            "--pbkdf2-iterations",
            "1000",
        ],
    );
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        stderr.contains("cannot be combined with --stealth"),
        "expected an explicit --optimise rejection, got: {stderr}"
    );

    let probe = Command::new("ffprobe")
        .current_dir(dir)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
            "stealth.mp4",
        ])
        .output()
        .unwrap();
    assert!(probe.status.success());
    assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "audio");

    success(cli(
        dir,
        &["decode", "stealth.mp4", "restored.bin", "--password", "a"],
    ));
    assert_eq!(fs::read(dir.join("restored.bin")).unwrap(), data);
}
