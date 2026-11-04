# Publishing the Rust crates

The `msb_krun` crates share one release version. Merge the version bump into
`krun` and confirm the platform CI checks pass before publishing.

## One-time setup

Configure the GitHub environment `crates-io`:

- Restrict deployments to the `krun` branch.
- Require a maintainer's approval.
- Add `CARGO_REGISTRY_TOKEN` as an environment secret. Use a crates.io token
  with permission to publish all `msb_krun` crates and keep its expiry current.

Do not store the token in repository files or pass it as a workflow input.

## Release

1. Open **Actions → Publish crates → Run workflow**.
2. Select `krun`, enter the merged version (for example, `0.1.42`), and leave
   **Publish to crates.io** off to validate the dependency plan.
3. Run again with **Publish to crates.io** enabled and approve the `crates-io`
   environment deployment.

Both jobs use the commit selected when the workflow starts, even if `krun`
advances while approval is pending. Runs from other branches are skipped.
Only one release workflow runs at a time.

The publisher validates versions and internal dependency requirements before
uploading. It includes optional and platform-specific dependencies in the order,
waits up to five minutes for each upload to appear in the crates.io index, and
stops on upload or registry errors. Rerun the same commit and version after
resolving a failure; existing non-yanked versions are skipped. This assumes
published versions came from the intended release; crates.io versions cannot be
overwritten. A yanked version requires maintainer intervention.

Uploads use `cargo publish --no-verify` because some crates need platform-specific
build environments. This does not replace the Linux, macOS and Windows PR checks;
confirm those checks before approving publication. The dry run validates the plan
only; it does not prove token permissions or registry package acceptance.

For a local plan check:

```sh
python3 scripts/publish-crates.py --version 0.1.42
python3 -m unittest discover -s scripts -p 'test_publish_crates.py'
```

This workflow publishes Rust crates only. It does not create a GitHub release,
publish firmware, or update Microsandbox's dependency versions.
