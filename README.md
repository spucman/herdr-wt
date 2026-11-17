# herdr-wt

Initialize a Git worktree with files and commands from the source checkout. Use `herdr-wt` as a [Herdr](https://herdr.dev/tui/docs/) plugin or run the same Rust binary directly.

**Status:** The first release has not been published. Build from source to try the plugin now.

## Install

Prebuilt packages target Linux and macOS on x86-64 and ARM64, and Windows on x86-64. Herdr integration requires Herdr 0.9.5+ and is preview on Windows. Installation does not require Rust or Cargo.

### Herdr-managed install

```sh
herdr plugin install spucman/herdr-wt
herdr plugin update herdr-wt
```

Herdr fetches `main`; the install hook selects and verifies the archived binary pinned there. Linux/macOS hooks use a shell and standard download, SHA-256, and archive tools; Windows uses PowerShell. No `--ref` or architecture argument is needed. Managed install is available once the first release is promoted, not merely uploaded.

### Link a downloaded release

Download the archive for your OS and CPU from [GitHub Releases](https://github.com/spucman/herdr-wt/releases), verify its SHA-256, and extract it to a persistent directory. Link that directory, not the binary:

```sh
herdr plugin link path/to/extracted-package
```

Herdr runs no install hook for linked packages. To update one, download the new archive, run `herdr plugin unlink herdr-wt`, and link the new directory. `herdr plugin update` only updates managed installs.

### Run standalone

Use the executable in the extracted archive directly, or put it on `PATH`. Standalone checkout discovery requires Git 2.36+; it does not require Herdr. The plugin also needs Git 2.36+ when it has to discover a bare repository’s source checkout, but not for ordinary Herdr runs with both checkout paths recorded.

```sh
herdr-wt run --dry-run /path/to/worktree
herdr-wt run --no-commands /path/to/worktree
herdr-wt run /path/to/worktree
```

On Windows, the executable is `herdr-wt.exe`. Omit the path to use the current directory; it can be a subdirectory of the target worktree.

## Configure and run

Copy [`.herdr-wt.example.toml`](.herdr-wt.example.toml) to `.herdr-wt.toml` at the root of the source checkout, normally the main checkout. The sample is also included in release archives. Replace the example paths with the files you want to initialize:

```toml
mode = "copy"
overwrite = false
files = [".env.local", "settings/", "fixtures/**/*.json"]
# commands = ["git status --short"]
```

The plugin reads files as they exist on disk, including ignored files and uncommitted edits. It rejects explicitly selected paths and config symlinks that resolve outside the source checkout. Links encountered inside a copied directory are recreated without reading their targets and can still point outside the worktree. Windows supports copy mode only; symlink mode on Linux/macOS uses absolute links that break if the source moves.

**Trust the repository before enabling automatic runs.** Configured commands run with your privileges, without a separate prompt. `overwrite = true` replaces a selected directory rather than merging it and can discard tracked edits and untracked files. Commands run again on a rerun, even when every existing destination is skipped. Use `--no-commands` to skip them or `--dry-run` to preview without writing or locking. `HERDR_WT_DRY_RUN=1` also enables dry run.

Herdr runs the plugin asynchronously only when it creates a worktree. Opening an existing worktree, restoring a workspace, or running `git worktree add` does not trigger it. To rerun in a Herdr-managed worktree workspace:

```sh
herdr plugin action invoke herdr-wt.run
herdr plugin log list --plugin herdr-wt
```

On Windows, invoke `herdr-wt.run-windows`. The action uses the workspace’s recorded checkout, not the focused pane’s current directory. If workspace metadata is unavailable, use the standalone command.

| Exit | Meaning |
| --- | --- |
| `0` | Success or expected no-op |
| `1` | Setup or preflight failure, including a busy target |
| `2` | File application failure |
| `3` | Command failure; takes precedence over file failures |
| `4` | Refused to initialize the source checkout |

If an interrupted replacement leaves a displaced original, the diagnostic names its path. Inspect and recover it before rerunning that destination. Do not treat it as disposable staging.

## Build from source

Developer builds require Rust 1.85+ with Cargo and Git 2.36+ for the worktree integration tests:

```sh
git clone https://github.com/spucman/herdr-wt.git
cd herdr-wt
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Run `target/release/herdr-wt --help` (`target/release/herdr-wt.exe` on Windows) to check the local build. Herdr integration tests also require Herdr 0.9.5+.

To test a local build in Herdr, copy the executable to `bin/herdr-wt` (`bin/herdr-wt.exe` on Windows) in this checkout and run `herdr plugin link .`.

## Release

Maintainers publish from [spucman/herdr-wt](https://github.com/spucman/herdr-wt) with `main` as its default branch. Allow GitHub Actions to open pull requests in repository settings. On `main`, require promotion PRs to be up to date before merge so an older release cannot supersede a newer pin.

1. Update the Cargo package version and lockfile, then merge the release commit into `main`. Do not update the install pin yet.
2. Tag the merged commit with the matching stable version and push the tag. For example, for Cargo version `0.1.0`:

   ```sh
   git tag -a v0.1.0 <merged-commit> -m "Release v0.1.0"
   git push origin v0.1.0
   ```

3. GitHub Actions builds and tests all five targets, verifies the download hooks, and publishes the archives and checksums. Before the first production release or after changes to install or promotion, rehearse a managed install and update in a test repository.
4. Approve checks if requested and merge the promotion PR that pins the published release and its digests on `main`. Managed users can now run `herdr plugin update herdr-wt`. Downloaded archives work for standalone and link installs once published.

To roll back managed installs, merge a reviewed PR restoring the previous version pin and digests, then tell users to run `herdr plugin update herdr-wt`. Do not move tags or replace release assets. Linked and standalone users can reinstall the previous archive.

## License

MIT.
