# Dependency audit: 2026-11-03

## Scope and method

This report follows [microsandbox#1997](https://github.com/superradcompany/microsandbox/issues/1997). It audits all 11 tracked Cargo lockfiles in the Microsandbox libkrun fork at base `a3c8e489bec03981bcd2014bea356d41886acb98`, plus both tracked lockfiles in Microsandbox at `311784756722866d934e7e641f6016a38df79495`. The fork's after results describe the dependency changes accompanying this report. Microsandbox was audited without modifications.

Tool: `cargo-audit 0.22.2`. RustSec advisory database: [`6c5fedf8ff03c485ed674d8f941e0180f3f72d70`](https://github.com/RustSec/advisory-db/commit/6c5fedf8ff03c485ed674d8f941e0180f3f72d70). Registry yanked-package checks were enabled. No advisory IDs, platforms, or architectures were excluded. Commands were run from an empty directory to avoid repository-local ignore configuration. Vulnerabilities, informational soundness/maintenance warnings, and yanked packages were all inspected.

To reproduce, check out the repository revision under review and the database revision above, then run this command for every path returned by `git ls-files '*Cargo.lock'` in each repository:

```sh
cargo audit --db /absolute/path/to/advisory-db --no-fetch --file /absolute/path/to/Cargo.lock --json
```

Run from an empty directory. `--no-fetch` freezes the advisory database, but registry yank status can change over time. The default audit exit code does not fail on informational warnings. Use `--deny warnings` to enforce those too. These lockfile scans include optional and platform-specific dependencies and are not proof that every affected interface is reachable in a deployed binary.

## Fixes in this change

All 15 published `msb_krun*` crate repository URLs now point to `superradcompany/libkrun`. The README identifies the maintained fork and explains that its crate versions are independent of upstream releases. Existing published crates retain their old metadata until new versions are published.

The VMM now requires `zstd >=0.13.4, <0.14`, whose dependency requires patched `zstd-safe >=7.4.0, <8`. The four tracked lockfiles containing zstd resolve `zstd 0.13.4`, `zstd-safe 7.4.0`, and `zstd-sys 2.1.2+zstd.1.5.7`. The AWS Nitro crate now requires `tar >=0.4.45, <0.5`, and the root lockfile resolves `0.4.45`. No runtime code or public APIs were changed.

| Package | Resolved advisories |
| --- | --- |
| zstd | RUSTSEC-2026-0391 |
| zstd-safe | RUSTSEC-2026-0395 through RUSTSEC-2026-0406 inclusive |
| zstd-sys | RUSTSEC-2026-0410 |
| tar | RUSTSEC-2026-0067, RUSTSEC-2026-0068 |

The root dependency paths are `msb_krun -> msb_krun_vmm -> zstd -> zstd-safe -> zstd-sys` and optional `msb_krun -> msb_krun_aws_nitro -> tar`. The VMM uses `zstd::stream::copy_decode` for x86-64 compressed kernels. It does not enable the seekable feature. `copy_decode` uses the read decoder, which RustSec explicitly excludes from the legacy write-decoder failure in RUSTSEC-2026-0403. RUSTSEC-2026-0410 concerns the wasm32 allocator, not the native VM targets. AWS Nitro uses tar archive creation rather than the extraction APIs in the two tar advisories. These exposure distinctions do not suppress the findings or replace installing patched versions.

The zstd-sys fix is a lockfile update, not a new direct dependency constraint. Downstream consumers retaining older lockfiles must update that transitive package too. Microsandbox also has a separate zstd 0.14 dependency through `compression-codecs`; publishing this fork alone will not update that path.

`examples/rust_vm` belongs to the root workspace, so Cargo uses the root lockfile when building it. Its redundant tracked lockfile was synchronized only for the three zstd package records, using the Cargo-resolved versions and checksums, so standalone lockfile scanners also see the fix. Separate memory-tracking and nested-restore test workspace locks were updated with Cargo.

## Fork results

Counts below are vulnerability occurrences / warning occurrences. The same advisory in multiple lockfiles is counted separately. There are no zstd or tar findings in any tracked fork lockfile after this change.

| Lockfile | Before | After |
| --- | --- | --- |
| `Cargo.lock` | 6 / 16 | 0 / 6 |
| `examples/Cargo.lock` | 0 / 3 | 0 / 3 |
| `examples/krun_gtk_display/Cargo.lock` | 0 / 3 | 0 / 3 |
| `examples/rust_vm/Cargo.lock` | 4 / 14 | 0 / 4 |
| `src/krun_input/Cargo.lock` | 0 / 0 | 0 / 0 |
| `tests/Cargo.lock` | 1 / 2 | 1 / 2 |
| `tests/memory-tracking/Cargo.lock` | 4 / 12 | 0 / 2 |
| `tests/nested-restore-hvf/Cargo.lock` | 0 / 0 | 0 / 0 |
| `tests/nested-restore/Cargo.lock` | 4 / 11 | 0 / 1 |
| `tests/nested-restore/clock-source/windows/Cargo.lock` | 0 / 0 | 0 / 0 |
| `tests/nested-restore/guest/Cargo.lock` | 0 / 0 | 0 / 0 |

The root audit exits successfully with zero vulnerabilities but still reports six informational warnings. The repository-wide scan is **not clean**: the integration-test workspace retains one vulnerability via `runner/test_cases -> tempdir -> remove_dir_all`. Replacing deprecated `tempdir` with `tempfile` requires a separate test-runner change. Other follow-ups are compatible patches for anyhow/rand/tokio, upgrading lru with cache-behavior validation, and deciding how to handle unmaintained bincode and atty. This PR does not change snapshot serialization formats or the integration-test runner.

### Remaining fork findings

| Advisory | Package versions | Class | Lockfiles |
| --- | --- | --- | --- |
| [RUSTSEC-2026-0190](https://rustsec.org/advisories/RUSTSEC-2026-0190.html) | `anyhow` 1.0.102, 1.0.95, 1.0.98 | unsound | `Cargo.lock`, `examples/Cargo.lock`, `examples/krun_gtk_display/Cargo.lock`, `tests/Cargo.lock` |
| [RUSTSEC-2021-0145](https://rustsec.org/advisories/RUSTSEC-2021-0145.html) | `atty` 0.2.14 | unsound | `examples/Cargo.lock`, `examples/krun_gtk_display/Cargo.lock` |
| [RUSTSEC-2024-0375](https://rustsec.org/advisories/RUSTSEC-2024-0375.html) | `atty` 0.2.14 | unmaintained | `examples/Cargo.lock`, `examples/krun_gtk_display/Cargo.lock` |
| [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141.html) | `bincode` 2.0.1 | unmaintained | `Cargo.lock`, `examples/rust_vm/Cargo.lock`, `tests/memory-tracking/Cargo.lock`, `tests/nested-restore/Cargo.lock` |
| [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002.html) | `lru` 0.14.0 | unsound | `Cargo.lock` |
| [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253.html) | `lru` 0.14.0, 0.16.3 | unsound | `Cargo.lock`, `examples/rust_vm/Cargo.lock` |
| [RUSTSEC-2026-0097](https://rustsec.org/advisories/RUSTSEC-2026-0097.html) | `rand` 0.9.2 | unsound | `Cargo.lock`, `examples/rust_vm/Cargo.lock` |
| [RUSTSEC-2023-0018](https://rustsec.org/advisories/RUSTSEC-2023-0018.html) | `remove_dir_all` 0.5.3 | vulnerability | `tests/Cargo.lock` |
| [RUSTSEC-2018-0017](https://rustsec.org/advisories/RUSTSEC-2018-0017.html) | `tempdir` 0.3.7 | unmaintained | `tests/Cargo.lock` |
| [RUSTSEC-2026-0386](https://rustsec.org/advisories/RUSTSEC-2026-0386.html) | `tokio` 1.45.1, 1.49.0, 1.53.1 | unsound | `Cargo.lock`, `examples/rust_vm/Cargo.lock`, `tests/memory-tracking/Cargo.lock` |

## Microsandbox downstream results

These are baseline findings, not fixed by this PR. Each of the two lockfiles has 16 vulnerability occurrences. Updating the published fork dependency is only part of the required downstream work.

| Lockfile | Vulnerabilities / warnings |
| --- | --- |
| `Cargo.lock` | 16 / 33 |
| `sdk/ruby/ext/microsandbox/Cargo.lock` | 16 / 14 |

Notable dependency paths from the main lockfile are `compression-codecs -> zstd 0.14`, `msb_krun_vmm -> zstd 0.13`, `rust_decimal -> rkyv`, `russh/ssh-key -> rsa`, and `quinn -> quinn-proto`. Crypto dependencies bring in crypto-bigint and ctutils. These are lockfile relationships, including optional dependencies; runtime reachability requires feature and call-site review. The rkyv fix crosses from 0.7 to 0.8, so serialization compatibility must be checked before upgrading. The RSA advisory lists no patched release; actual private-key operations and mitigations need review rather than an unsupported claim that a version update fixes it.

The Ruby extension additionally retains h2 and webpki root-store advisories. Its lockfile must be updated independently. Yanked packages are included below because a successful ordinary vulnerability check would otherwise hide them.

### All downstream findings

| Advisory | Package versions | Class | Lockfiles |
| --- | --- | --- | --- |
| Registry yank | `aes` 0.9.0 | yanked | `Cargo.lock` |
| [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141.html) | `bincode` 2.0.1 | unmaintained | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| Registry yank | `chacha20` 0.10.0, 0.10.1 | yanked | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| Registry yank | `crypto-bigint` 0.7.5 | yanked | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387.html) | `crypto-bigint` 0.7.5 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0385](https://rustsec.org/advisories/RUSTSEC-2026-0385.html) | `ctutils` 0.4.2 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| Registry yank | `der` 0.8.0 | yanked | `Cargo.lock` |
| [RUSTSEC-2026-0221](https://rustsec.org/advisories/RUSTSEC-2026-0221.html) | `event-listener` 5.4.1 | unsound | `Cargo.lock` |
| [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html) | `h2` 0.4.15 | vulnerability | `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0173](https://rustsec.org/advisories/RUSTSEC-2026-0173.html) | `proc-macro-error2` 2.0.1 | unmaintained | `Cargo.lock` |
| [RUSTSEC-2026-0338](https://rustsec.org/advisories/RUSTSEC-2026-0338.html) | `quinn-proto` 0.11.16 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0339](https://rustsec.org/advisories/RUSTSEC-2026-0339.html) | `quinn-proto` 0.11.16 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0340](https://rustsec.org/advisories/RUSTSEC-2026-0340.html) | `quinn-proto` 0.11.16 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0341](https://rustsec.org/advisories/RUSTSEC-2026-0341.html) | `quinn-proto` 0.11.16 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0342](https://rustsec.org/advisories/RUSTSEC-2026-0342.html) | `quinn-proto` 0.11.16 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0235](https://rustsec.org/advisories/RUSTSEC-2026-0235.html) | `rkyv` 0.7.46 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) | `rsa` 0.10.0-rc.18 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0249](https://rustsec.org/advisories/RUSTSEC-2026-0249.html) | `smartstring` 1.0.1 | unmaintained | `Cargo.lock` |
| Registry yank | `spin` 0.9.8 | yanked | `Cargo.lock` |
| [RUSTSEC-2026-0383](https://rustsec.org/advisories/RUSTSEC-2026-0383.html) | `webpki-root-certs` 1.0.9 | vulnerability | `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381.html) | `webpki-roots` 1.0.9 | vulnerability | `sdk/ruby/ext/microsandbox/Cargo.lock` |
| Registry yank | `wnaf` 0.14.0 | yanked | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0371](https://rustsec.org/advisories/RUSTSEC-2026-0371.html) | `zbus_names` 3.0.0 | unmaintained | `Cargo.lock` |
| [RUSTSEC-2026-0391](https://rustsec.org/advisories/RUSTSEC-2026-0391.html) | `zstd` 0.13.3, 0.14.0 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0395](https://rustsec.org/advisories/RUSTSEC-2026-0395.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0396](https://rustsec.org/advisories/RUSTSEC-2026-0396.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0397](https://rustsec.org/advisories/RUSTSEC-2026-0397.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0398](https://rustsec.org/advisories/RUSTSEC-2026-0398.html) | `zstd-safe` 7.2.4, 8.0.0 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0399](https://rustsec.org/advisories/RUSTSEC-2026-0399.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0400](https://rustsec.org/advisories/RUSTSEC-2026-0400.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0401](https://rustsec.org/advisories/RUSTSEC-2026-0401.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0402](https://rustsec.org/advisories/RUSTSEC-2026-0402.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0403](https://rustsec.org/advisories/RUSTSEC-2026-0403.html) | `zstd-safe` 7.2.4, 8.0.0 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0404](https://rustsec.org/advisories/RUSTSEC-2026-0404.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0405](https://rustsec.org/advisories/RUSTSEC-2026-0405.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0406](https://rustsec.org/advisories/RUSTSEC-2026-0406.html) | `zstd-safe` 7.2.4, 8.0.0 | unsound | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0410](https://rustsec.org/advisories/RUSTSEC-2026-0410.html) | `zstd-sys` 2.0.16+zstd.1.5.7, 2.1.0+zstd.1.5.7 | vulnerability | `Cargo.lock`, `sdk/ruby/ext/microsandbox/Cargo.lock` |
| [RUSTSEC-2026-0370](https://rustsec.org/advisories/RUSTSEC-2026-0370.html) | `zvariant` 4.2.0 | unmaintained | `Cargo.lock` |
| [RUSTSEC-2026-0372](https://rustsec.org/advisories/RUSTSEC-2026-0372.html) | `zvariant_derive` 4.2.0 | unmaintained | `Cargo.lock` |

## Validation and limits

- `cargo metadata --no-deps --offline --format-version 1`: all 15 published crate repository URLs verified.
- `cargo test --locked -p msb_krun -p msb_krun_vmm --features msb_krun/net,msb_krun/blk --lib`: 126 tests passed on macOS aarch64.
- A temporary compatibility harness using exact zstd 0.13.4 and tar 0.4.45 passed three tests: zstd round-trip decoding, invalid/truncated-frame rejection, and deterministic tar archive round-trip. This is supplemental API validation, not a VM boot test.
- `cargo clippy --locked -p msb_krun -p msb_krun_vmm --features msb_krun/net,msb_krun/blk -- -D warnings`: passed on macOS aarch64.
- Every tracked fork lockfile was re-audited with the same database snapshot; all zstd/tar findings are removed. No ignore file was added.

A newly published fork release and downstream lockfile updates are still required to deliver these changes to registry consumers. Linux/Windows CI and actual x86-64 VM boot coverage remain separate from the local unit tests. This audit does not certify the absence of unknown vulnerabilities or assess dependencies outside Cargo.
