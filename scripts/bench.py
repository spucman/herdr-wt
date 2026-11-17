"""Measure real CLI preflight and copy times without touching a user's checkout."""
import argparse
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def main(binary, count, repeats):
    binary = binary.resolve(strict=True)
    with tempfile.TemporaryDirectory() as tmp:
        source, target = Path(tmp) / "main", Path(tmp) / "linked"
        source.mkdir()
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=source, check=True)
        subprocess.run(["git", "-c", "user.name=Bench", "-c", "user.email=bench@example.org",
                        "-c", "commit.gpgSign=false", "commit", "-q", "--allow-empty", "-m", "seed"], cwd=source, check=True)
        subprocess.run(["git", "worktree", "add", "-q", "--detach", target], cwd=source, check=True)
        for i in range(count):
            path = source / "fixtures" / str(i % 50) / f"{i}.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"{}\n")
        for selector in ["fixtures", "fixtures/**/*.json"]:
            (source / ".herdr-wt.toml").write_text(f'files = ["{selector}"]\noverwrite = true\n')
            for flags in [["--dry-run"], []]:
                # Warm up before measuring steady-state replacements.
                times = []
                for _ in range(repeats + 1):
                    start = time.perf_counter()
                    subprocess.run([binary, "run", *flags, target], check=True, stdout=subprocess.DEVNULL)
                    times.append(time.perf_counter() - start)
                print(f"{selector}: {flags or 'apply'} {count} files, median {statistics.median(times[1:]):.3f}s")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("binary", type=Path)
    p.add_argument("--files", type=int, default=2000)
    p.add_argument("--repeats", type=int, default=3)
    args = p.parse_args()
    if args.files < 1 or args.repeats < 1:
        p.error("counts must be positive")
    main(args.binary, args.files, args.repeats)
