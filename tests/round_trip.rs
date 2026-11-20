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
