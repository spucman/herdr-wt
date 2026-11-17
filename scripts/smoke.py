"""Smoke-test a packaged executable and the Herdr 0.9.5 integration."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import time
import traceback
import zipfile

from release import ROOT, OUT, TARGETS, asset, manifest, pin_text, sha256

HOSTS = {
    "linux-x86_64": "14d9e8bc8b4a9e0118d77c32266f76d461f09b40342eb23752960bd89f59929d",
    "linux-aarch64": "17d7536f2da50859523680ef279dffcac8e4a235f4e46fe8da6c2ec44141897c",
    "macos-x86_64": "5fb0a908ec7f6745e3e245738b06bfad56c46391c391ab3c0955fbead01f1369",
    "macos-aarch64": "fb77c2232a05a8035421382c894392f4174e4e38a18e48af3dd0290157c3a7e2",
    "windows-x86_64": "d2c6c9c971c43a0bfdde8c692a9a1a7a45c3e4807cd01ae45662b4734fac1cbf",
}


def call(*args, cwd, env, ok=True):
    p = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=30)
    if ok and p.returncode:
        raise RuntimeError(f"{args} -> {p.returncode}\n{p.stdout}\n{p.stderr}")
    return p


def fixture(folder, env):
    folder.mkdir()
    for args in [
        ("init", "-b", "main"),
        ("-c", "user.name=Smoke", "-c", "user.email=smoke@example.org", "commit", "--allow-empty", "-m", "seed"),
    ]:
        call("git", *args, cwd=folder, env=env)
    (folder / ".herdr-wt.toml").write_text('files = ["fixture.txt"]\n', encoding="utf-8")
    (folder / "fixture.txt").write_text("copied\n", encoding="utf-8")
    return folder


def wait(check):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.25)
    raise RuntimeError("timed out waiting for Herdr")


def host(root, target, override):
    if override:
        return override.resolve(strict=True)
    name = f"herdr-{target}" + (".zip" if os.name == "nt" else "")
    path = root / name
    subprocess.run(
        ["gh", "release", "download", "v0.9.5", "--repo", "herdrdev/herdr", "--pattern", name, "--output", path],
        check=True,
    )
    if sha256(path) != HOSTS[target]:
        raise RuntimeError("upstream Herdr SHA-256 mismatch")
    if os.name == "nt":
        with zipfile.ZipFile(path) as archive:
            archive.extractall(root / "herdr")
        return root / "herdr" / "herdr.exe"
    path.chmod(0o755)
    return path


def verify_managed(root, target, tag, env, prefix, request, old_archive):
    """Exercise Herdr's real Git install/update path against an isolated fixture."""
    repo = root / "remote"
    repo.mkdir()
    assets = root / "release-assets"
    assets.mkdir()
    name = asset(target)
    shutil.copy2(old_archive, assets / name)
    shutil.copytree(ROOT / "scripts", repo / "scripts", ignore=shutil.ignore_patterns("__pycache__"))
    call("git", "init", "-b", "main", cwd=repo, env=env)
    env.update({
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": f"url.{repo.as_uri()}.insteadOf",
        "GIT_CONFIG_VALUE_0": "https://github.com/example/herdr-wt.git",
        "HERDR_WT_TEST_ASSET_DIR": str(assets),
    })
    template = (ROOT / "herdr-plugin.toml").read_text(encoding="utf-8")

    def promote(version):
        (repo / "herdr-plugin.toml").write_text(manifest(template, version), encoding="utf-8")
        (repo / "release-pin").write_text(
            pin_text(version, "example/herdr-wt", assets, [target]), encoding="ascii", newline="\n"
        )
        call("git", "add", "herdr-plugin.toml", "release-pin", "scripts", cwd=repo, env=env)
        call(
            "git", "-c", "user.name=Smoke", "-c", "user.email=smoke@example.org",
            "-c", "commit.gpgSign=false", "commit", "-m", version, cwd=repo, env=env,
        )

    def installed():
        return request("plugin", "list", "--plugin", "herdr-wt", "--json")["plugins"][0]

    request("plugin", "unlink", "herdr-wt")
    promote("v0.0.0")
    call(*prefix, "plugin", "install", "example/herdr-wt", "--yes", cwd=root, env=env)
    before = installed()
    assert before["version"] == "0.0.0", before
    assert before["source"]["kind"] == "github", before
    binary = "bin/herdr-wt.exe" if os.name == "nt" else "bin/herdr-wt"
    previous_binary = Path(before["plugin_root"]) / binary
    assert call(previous_binary, "--version", cwd=root, env=env).stdout.strip() == "herdr-wt fixture 0.0.0"
    previous_hash = sha256(previous_binary)
    shutil.copy2(OUT / name, assets / name)
    promote(tag)
    (assets / name).write_bytes(b"corrupt package")
    failed = call(*prefix, "plugin", "update", "herdr-wt", "--yes", cwd=root, env=env, ok=False)
    assert failed.returncode and "checksum mismatch" in (failed.stdout + failed.stderr), failed
    assert installed()["plugin_root"] == before["plugin_root"]
    shutil.copy2(OUT / name, assets / name)
    call(*prefix, "plugin", "update", "herdr-wt", "--yes", cwd=root, env=env)
    after = installed()
    assert after["version"] == tag[1:], after
    assert after["source"]["resolved_commit"] != before["source"]["resolved_commit"], after
    updated = Path(after["plugin_root"]) / binary
    assert sha256(updated) != previous_hash
    assert call(updated, "--version", cwd=root, env=env).stdout.strip() == f"herdr-wt {tag[1:]}"
    no_change = call(*prefix, "plugin", "update", "herdr-wt", "--yes", cwd=root, env=env)
    assert "already up to date" in no_change.stdout
    assert installed()["plugin_root"] == after["plugin_root"]


def verify_herdr(root, package, env, target, override, tag, old_archive):
    binary = host(root, target, override)
    for key in list(env):
        if key.startswith("HERDR_"):
            env.pop(key)
    env["XDG_CONFIG_HOME"] = str(root / "c")
    env["XDG_STATE_HOME"] = str(root / "s")
    conf = root / "c" / "herdr"
    conf.mkdir(parents=True)
    (conf / "config.toml").write_text("onboarding = false\n", encoding="utf-8")
    if os.name != "nt":
        env["HERDR_SOCKET_PATH"] = str(root / "server.sock")
    source = fixture(root / "source", env)
    target_dir = root / "wt"
    prefix = [str(binary), "--session", "smoke"]

    def request(*args, ready=False):
        p = call(*prefix, *args, cwd=root, env=env, ok=not ready)
        if ready and p.returncode:
            return None
        return json.loads(p.stdout)["result"]

    def successful_log(match):
        records = request("plugin", "log", "list", "--plugin", "herdr-wt", "--limit", "20")["logs"]
        for record in records:
            if match(record) and record["status"] != "running":
                assert record["status"] == "succeeded" and record.get("exit_code") == 0, record
                return record
        return None

    log = root / "server.log"
    with log.open("w+") as stream:
        p = subprocess.Popen([*prefix, "server"], cwd=root, env=env, stdout=stream, stderr=stream)
        try:
            wait(lambda: (request("workspace", "list", ready=True) or {}).get("type") == "workspace_list")
            assert request("plugin", "link", str(package))["type"] == "plugin_linked"
            source_ws = request("workspace", "create", "--cwd", str(source), "--focus")["workspace"]["workspace_id"]
            ws = request(
                "worktree", "create", "--workspace", source_ws, "--branch", "smoke-wt",
                "--base", "HEAD", "--path", str(target_dir), "--focus",
            )["workspace"]["workspace_id"]
            wait(lambda: successful_log(lambda r: r.get("event") == "worktree.created"))
            copied = target_dir / "fixture.txt"
            assert copied.read_text() == "copied\n"
            copied.unlink()
            action = "run-windows" if os.name == "nt" else "run"
            invoked = request("plugin", "action", "invoke", f"herdr-wt.{action}")
            assert invoked["context"]["workspace_id"] == ws, invoked
            wait(lambda: successful_log(lambda r: r["log_id"] == invoked["log"]["log_id"]))
            assert copied.read_text() == "copied\n"
            verify_managed(root, target, tag, env, prefix, request, old_archive)
        except Exception:
            print(log.read_text(errors="replace"))
            raise
        finally:
            try:
                call(*prefix, "server", "stop", cwd=root, env=env, ok=False)
                p.wait(timeout=10)
            finally:
                if p.poll() is None:
                    p.kill()
                    p.wait(timeout=5)


def main(tag, target, herdr, skip_host):
    windows = os.name == "nt"
    exe = "herdr-wt.exe" if windows else "herdr-wt"
    name = asset(target)
    temp_parent = Path("/tmp") if not windows else None
    with tempfile.TemporaryDirectory(prefix="hwt-", dir=temp_parent) as tmp:
        root = Path(tmp).resolve()
        env = dict(os.environ)
        # Keep Git and platform tools, but remove Cargo from the test PATH.
        env["PATH"] = os.pathsep.join(p for p in env["PATH"].split(os.pathsep) if not shutil.which("cargo", path=p))
        assert not shutil.which("cargo", path=env["PATH"])
        package = root / "package"
        package.mkdir()
        if windows:
            with zipfile.ZipFile(OUT / name) as archive:
                archive.extractall(package)
        else:
            with tarfile.open(OUT / name) as archive:
                archive.extractall(package, filter="data")
        binary = package / "bin" / exe
        version_output = f"herdr-wt {tag[1:]}"
        assert call(binary, "--version", cwd=root, env=env).stdout.strip() == version_output
        source = fixture(root / "standalone", env)
        wt = root / "standalone-wt"
        call("git", "worktree", "add", "-b", "second", str(wt), cwd=source, env=env)
        call(binary, "run", wt, cwd=root, env=env)
        assert (wt / "fixture.txt").read_text() == "copied\n"

        install = root / "install"
        install.mkdir()
        shutil.copytree(ROOT / "scripts", install / "scripts")
        pin = pin_text(tag, "example/herdr-wt", OUT, [target])
        (install / "release-pin").write_text(pin, encoding="ascii", newline="\n")
        install_env = {**env, "HERDR_WT_TEST_ASSET_DIR": str(OUT)}
        command = (
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", "scripts/install.ps1"]
            if windows else ["sh", "scripts/install.sh"]
        )
        call(*command, cwd=install, env=install_env)
        installed = install / "bin" / exe
        assert call(installed, "--version", cwd=root, env=env).stdout.strip() == version_output
        fake = root / "assets"
        fake.mkdir()
        (fake / name).write_bytes(b"not the published package")
        error = call(*command, cwd=install, env={**install_env, "HERDR_WT_TEST_ASSET_DIR": str(fake)}, ok=False)
        assert error.returncode and "checksum mismatch" in (error.stdout + error.stderr), error
        assert sha256(installed) == sha256(binary)
        if not skip_host:
            # Compile a distinct bootstrap fixture in the harness, before
            # handing the Cargo-free environment to any installer or plugin.
            fixture_src = root / "old.rs"
            fixture_src.write_text('fn main() { println!("herdr-wt fixture 0.0.0"); }', encoding="ascii")
            old_binary = root / exe
            subprocess.run(["rustc", fixture_src, "-o", old_binary], check=True)
            old_archive = root / name
            if windows:
                with zipfile.ZipFile(old_archive, "w") as archive:
                    archive.write(old_binary, f"bin/{exe}")
            else:
                with tarfile.open(old_archive, "w:gz") as archive:
                    archive.add(old_binary, f"bin/{exe}")
            verify_herdr(root, package, env, target, herdr, tag, old_archive)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("tag")
    parser.add_argument("target", choices=TARGETS)
    parser.add_argument("--herdr", type=Path)
    parser.add_argument("--skip-host", action="store_true", default=False)
    try:
        main(**vars(parser.parse_args()))
    except Exception as exc:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            frames = traceback.extract_tb(exc.__traceback__)
            frame = next(f for f in reversed(frames) if f.filename == __file__)
            message = f"{frame.name}: {type(exc).__name__}: {exc}"
            message = message[:2000].replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
            print(f"::error file=scripts/smoke.py,line={frame.lineno}::{message}", flush=True)
        raise
