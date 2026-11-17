"""Measure unique resolved crates per release target; audit all locked versions."""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tomllib
import urllib.request

from release import ROOT, TARGETS


def graph(target, edges):
    rows = subprocess.check_output([
        "cargo", "tree", "--locked", "--target", target, "--edges", edges,
        "--prefix", "none", "--format", "{p}", "--no-dedupe",
    ], cwd=ROOT, text=True).splitlines()
    # The application itself is not a dependency. Keep package versions.
    return {" ".join(row.split()[:2]) for row in rows if not row.startswith("herdr-wt v")}


def report(base=None):
    targets = {}
    for _, triple in TARGETS.values():
        production = graph(triple, "normal,build")
        with_tests = graph(triple, "normal,build,dev")
        targets[triple] = {"production": sorted(production), "dev_only": sorted(with_tests - production)}
    print("| Target | Production (incl. build) | Additional for tests |")
    print("| --- | ---: | ---: |")
    for triple, sets in targets.items():
        print(f"| `{triple}` | {len(sets['production'])} | {len(sets['dev_only'])} |")
    if base:
        previous = json.loads(base.read_text())
        for triple, sets in targets.items():
            for kind, current in sets.items():
                before = set(previous.get(triple, {}).get(kind, []))
                for label, change in [("added", set(current) - before), ("removed", before - set(current))]:
                    if change:
                        print(f"\n{triple} / {kind} {label}: " + ", ".join(sorted(change)))
    return targets


def audit():
    with (ROOT / "Cargo.lock").open("rb") as lock:
        packages = [p for p in tomllib.load(lock)["package"] if p.get("source", "").startswith("registry+")]
    queries = [{"version": p["version"], "package": {"ecosystem": "crates.io", "name": p["name"]}} for p in packages]
    req = urllib.request.Request("https://api.osv.dev/v1/querybatch",
                                 data=json.dumps({"queries": queries}).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as response:
        results = json.load(response)["results"]
    findings = []
    for package, result in zip(packages, results, strict=True):
        for vuln in result.get("vulns", []):
            findings.append(f"{package['name']} {package['version']}: {vuln['id']}")
    if findings:
        sys.exit("\n".join(findings))
    print(f"No known advisories for {len(packages)} locked registry packages.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--json", type=Path)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--audit", action="store_true")
    args = parser.parse_args()
    if args.audit:
        audit()
    else:
        sets = report(args.compare)
        if args.json:
            args.json.write_text(json.dumps(sets, indent=2) + "\n")
