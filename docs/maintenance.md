# Maintaining herdr-wt

Runtime code is one Rust crate. `checkout` resolves checkouts and parses Herdr payloads, `config` decodes TOML, `paths` validates source paths, and `plan` resolves selections and rejects collisions before mutation. `fs` holds directory-handle operations and checkout identity. `apply` copies content, `replace` owns staging and recovery, `lock` coordinates mutating runs, and `commands` executes repository commands.

## Invariants

| Invariant | Regression test |
| --- | --- |
| File application cannot write into the source checkout or displace it by replacing its ancestor. | `nested_source_is_never_a_file_destination` |
| A failed install restores the old destination if its name is free. A new arrival is never overwritten by no-clobber publication or restoration. | `swap_restores_original_if_publish_fails`, `publication_and_restore_never_replace_a_new_arrival` |
| Recovery data survives changes to the configured selection, including replacing a parent of the affected destination. | `overwrite_and_displaced_original`, `replacing_parent_preserves_nested_recovery_data` |
| A command-only run holds the same target lock as a copy; a preview can proceed while the lock is held. | `lock_spans_commands_but_not_preview` |
| Case-distinct names remain distinct on case-sensitive checkout volumes. | `case_policy_matches_the_checkout_volume`, `case_sensitive_targets_keep_distinct_names` |
| Invalid `files` entries fail before any file or command runs. | `validation_precedes_files_and_commands`, `invalid_entry_error_names_rule_and_entry` |
| Glob selection never follows symlinks or descends into nested repositories, and warns once per skipped link. | `globs_skip_symlinks_and_nested_repositories`, `glob_symlink_warning_is_printed_once` |
| Special files are skipped without being opened. | `top_level_fifo_is_skipped_without_blocking` |
| Panics and broken output pipes exit 1. | `broken_stdout_exits_one` |
| A pre-existing target directory that the current user cannot write is never replaced and never chmod'ed. | `read_only_existing_dir_is_not_replaced` |
| Only herdr-wt's own `.herdr-wt-tmp-stage-*` and `.herdr-wt-tmp-original-*` entries are made writable before deletion; any other name is never chmod'ed. | `remove_never_chmods_unprefixed_entries`, `remove_makes_prefixed_read_only_trees_writable` |
| Cleanup of a displaced original continues past unreadable sibling directories among read-only ones (Linux only for directories without the read bit). | `remove_handles_unreadable_siblings_among_read_only_directories` |
| Making a tree writable never follows symlinks. | `make_writable_never_follows_links` |
| Read-only directories can be copied and replaced without leaving staging or backup data. | `read_only_source_dir_is_copied`, `overwrite_of_tree_with_read_only_subdir_leaves_no_backup` |

`validation_precedes_files_and_commands` lives in `tests/worktrees.rs`; the `remove_*` and `make_writable_*` tests are unit tests in `src/replace.rs`.

## Permissions and failure behavior

- The read-only check on an existing target directory is Unix-only (`access(W_OK)` relative to the target root). A pre-existing target directory that is not writable by the current user is never replaced and never chmod'ed. herdr-wt makes only its own `.herdr-wt-tmp-stage-*` and `.herdr-wt-tmp-original-*` trees writable, including read-only subdirectories of a displaced original, immediately before deleting them. It never follows symlinks while doing so.
- Making directories without the read bit writable before cleanup works on Linux only, through `/proc/self/fd`. On other Unix platforms such directories can remain and block later overwrites until fixed by hand.
- The mode of a replaced top-level directory is applied after publication through a directory handle opened before publication, so no name is resolved again.
- The panic hook replaces Rust's default hook: `RUST_BACKTRACE` has no effect, the panic is reported as a single `error:` line, and the exit code is 1.

User-visible output wording is asserted by integration tests, and `--version` must print exactly `herdr-wt <version>` because `scripts/smoke.py` compares it.

Name matching uses the target root’s Git marker to infer case sensitivity without writing a probe. Unicode uppercasing and macOS normalization are conservative approximations; a filesystem with mixed per-directory policies or different Unicode rules may be rejected even if it can store both names. Cover each supported host in CI. Do not change persistent lock/recovery key formats without migration.

## Dependencies

Run `python3 scripts/dependencies.py` to count unique package versions **separately for each release target**. Production includes normal and build dependencies (including proc macros); the test column is the incremental normal/build/dev graph. Counts are not binary size. Use `--json report.json` to retain package lists and `--compare base.json` to see version and membership changes. `--audit` checks all locked registry versions against OSV.

Dependabot checks Cargo and GitHub Actions weekly. The dependency workflow reports base-to-PR changes and runs a scheduled advisory check. Keep Rust 1.88 CI green when updating dependencies; do not reject a crate solely for a `0.x` version. Herdr's minimum-host smoke test intentionally stays pinned to 0.9.5 and its asset digests until the supported minimum changes.

## Package verification and performance

PRs changing packaging run native install/link/update smoke tests. Each release platform uploads the exact archive that passed testing and its digest receipt. Publication and promotion compare downloaded bytes with those receipts. Retry **failed jobs** to reuse that run’s artifacts; a fresh build may have different archive timestamps and must not silently replace a published asset.

For performance changes, benchmark the release executable against disposable worktrees:

```sh
cargo build --release --locked
python3 scripts/bench.py target/release/herdr-wt --files 2000 --repeats 3
```

Record the OS, filesystem, file count, and medians before and after the change. Compare directory selection and glob selection separately.
