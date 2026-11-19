"""Compare both CLI directions against a pinned Bit2Vid checkout (CI)."""
import os
from pathlib import Path
import random
import subprocess
import sys
import tempfile


def ffmpeg(args, data=None):
    return subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", *args],
        input=data, stdout=subprocess.PIPE, check=True,
    ).stdout


def check_pixel_equivalence():
    import numpy as np

    grid = np.random.default_rng(47).integers(0, 2, (45, 81), dtype=np.uint8) * 255
    expanded = grid.repeat(8, axis=0).repeat(8, axis=1)
    prefix = ["-f", "rawvideo", "-pix_fmt", "gray", "-threads", "1", "-s"]
    suffix = ["-frames:v", "1", "-threads:v", "1", "-pix_fmt", "yuv420p", "-f", "rawvideo", "pipe:1"]
    old = ffmpeg([*prefix, "648x360", "-i", "pipe:0", *suffix], expanded.tobytes())
    new = ffmpeg([
        *prefix, "81x45", "-i", "pipe:0", "-vf", "scale=648:360:flags=neighbor,setsar=1",
        *suffix,
    ], grid.tobytes())
    assert old == new, "the old and new frame expanders must feed identical pixels to x264"


def damage_first_rs_word(source, dest):
    width, height, block = 648, 360, 8
    cols, rows = width // block, height // block
    raw = bytearray(ffmpeg([
        "-i", str(source), "-threads:v", "1", "-pix_fmt", "gray", "-f", "rawvideo", "pipe:1"
    ]))
    # Skip the repeated header (448 bytes) and RS metadata (30 bytes); flip
    # one data bit in each of 32 distinct symbols of the first RS codeword.
    for entry in range(32):
        bit = (448 + 30 + entry) * 8
        frame, cell = divmod(bit, cols * rows)
        y, x = divmod(cell, cols)
        for line in range(block):
            start = (frame * height + y * block + line) * width + x * block
            raw[start:start + block] = bytes([255 if raw[start] < 128 else 0]) * block
    ffmpeg([
        "-f", "rawvideo", "-pix_fmt", "gray", "-s", "648x360", "-r", "30", "-i", "pipe:0",
        "-c:v", "libx264", "-preset", "ultrafast", "-crf", "0", "-pix_fmt", "yuv420p", str(dest),
    ], raw)


def main():
    check_pixel_equivalence()
    binary, upstream_src = (Path(value).resolve() for value in sys.argv[1:])
    env = dict(os.environ, PYTHONPATH=str(upstream_src))
    # Upstream multiprocessing uses host logical CPUs rather than a runner quota.
    reference = [
        sys.executable, "-c",
        "import multiprocessing, runpy; multiprocessing.cpu_count=lambda: 2;"
        "runpy.run_module('bit2vid.cli', run_name='__main__')",
    ]
    rust = [str(binary), "--threads", "2"]
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        payload = random.Random(24586).randbytes(38_004)
        (tmp / "données.bin").write_bytes(payload)

        def run(program, *args):
            subprocess.run([*program, *args, "--password", "généré"], env=env,
                           cwd=tmp, check=True)

        run(reference, "encode", "données.bin", "python.mp4", "--pbkdf2-iterations", "1000")
        (tmp / "restored").mkdir()
        run(rust, "decode", "python.mp4", "restored")
        assert (tmp / "restored" / "données.bin").read_bytes() == payload

        run(rust, "encode", "données.bin", "rust.mp4", "--width", "648", "--height", "360",
            "--preset", "fast", "--v1", "--pbkdf2-iterations", "1000")
        run(reference, "decode", "rust.mp4", "python-restored.bin", "--width", "648",
            "--height", "360")
        assert (tmp / "python-restored.bin").read_bytes() == payload
        damage_first_rs_word(tmp / "rust.mp4", tmp / "damaged.mp4")
        run(rust, "decode", "damaged.mp4", "repaired.bin")
        assert (tmp / "repaired.bin").read_bytes() == payload


if __name__ == "__main__":
    main()
