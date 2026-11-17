# Maintaining herdr-wt

Runtime code is one Rust crate. `config` decodes TOML, `paths` validates source paths, and `plan` resolves selections and rejects collisions before mutation. `fs` holds directory-handle operations and checkout identity. `apply` copies content, `replace` owns staging and recovery, `lock` coordinates mutating runs, and `commands` executes repository commands.

## Invariants

| Invariant | Regression test |
| --- | --- |
| File application cannot write into the source checkout or displace it by replacing its ancestor. | `nested_source_is_never_a_file_destination` |
| A failed install restores the old destination if its name is free. A new arrival is never overwritten by no-clobber publication or restoration. | `swap_restores_original_if_publish_fails`, `publication_and_restore_never_replace_a_new_arrival` |
| Recovery data survives changes to the configured selection, including replacing a parent of the affected destination. | `overwrite_and_displaced_original`, `replacing_parent_preserves_nested_recovery_data` |
| A command-only run holds the same target lock as a copy; a preview can proceed while the lock is held. | `lock_spans_commands_but_not_preview` |
| Case-distinct names remain distinct on case-sensitive checkout volumes. | `case_policy_matches_the_checkout_volume`, `case_sensitive_targets_keep_distinct_names` |

Name matching uses the target root’s Git marker to infer case sensitivity without writing a probe. Unicode uppercasing and macOS normalization are conservative approximations; a filesystem with mixed per-directory policies or different Unicode rules may be rejected even if it can store both names. Cover each supported host in CI. Do not change persistent lock/recovery key formats without migration.

## Dependencies

Run `python3 scripts/dependencies.py` to count unique package versions **separately for each release target**. Production includes normal and build dependencies (including proc macros); the test column is the incremental normal/build/dev graph. Counts are not binary size. Use `--json report.json` to retain package lists and `--compare base.json` to see version and membership changes. `--audit` checks all locked registry versions against OSV.

Dependabot checks Cargo and GitHub Actions weekly. The dependency workflow reports base-to-PR changes and runs a scheduled advisory check. Keep Rust 1.85 CI green when updating dependencies; do not reject a crate solely for a `0.x` version. Herdr's minimum-host smoke test intentionally stays pinned to 0.9.5 and its asset digests until the supported minimum changes.

## Package verification and performance

PRs changing packaging run native install/link/update smoke tests. Each release platform uploads the exact archive that passed testing and its digest receipt. Publication and promotion compare downloaded bytes with those receipts. Retry **failed jobs** to reuse that run’s artifacts; a fresh build may have different archive timestamps and must not silently replace a published asset.

For performance changes, benchmark the release executable against disposable worktrees:

```sh
cargo build --release --locked
python3 scripts/bench.py target/release/herdr-wt --files 2000 --repeats 3
```

Record the OS, filesystem, file count, and medians before and after the change. Compare directory selection and glob selection separately.
