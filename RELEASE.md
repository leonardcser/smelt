# Release

Releases are built and published from immutable tags. Committed Cargo versions
remain development versions; the release workflow derives the agent version
from the tag and materializes it only in each runner checkout. The workflow
never edits `main`, creates commits, pushes generated changes, or moves tags.

## Rules

- Do not prepare or commit an agent version bump. CI owns release versioning.
- Agent releases use SemVer tags such as `v0.5.0-alpha.13` or `v0.6.0`.
  Tags with prerelease suffixes produce GitHub prereleases.
- Tag the exact commit that passed CI, from a clean and up-to-date `main`.
- Never move, force-push, delete, or reuse a release tag.
- If publishing fails after a version becomes externally visible, prepare a new
  version and create a new tag.

## Agent binary

1. Choose the next normal SemVer version. Do not edit Cargo manifests or
   lockfiles for the agent release.
2. Write the changelog. Run

   ```bash
   scripts/changelog.py draft <X.Y.Z>
   ```

   It writes `docs/docs/changelog/<X.Y.Z>.md` from the `feat` and `fix` commits
   since the last stable tag and updates the index and sidebar. Curate the
   page for users: merge duplicates, drop internal items, reword, and replace
   the `TODO` summary with one line of highlights. Merge that to `main`. The
   release workflow publishes the page as the GitHub release notes and fails
   when it is missing or still has the draft summary. Pre-release tags need no
   page.
3. Run the full validation suite on the exact source commit:

   ```bash
   cargo fmt -- --check
   cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings
   cargo llvm-cov nextest --workspace --features smelt-tui/harness --fail-under-lines 80
   cargo xtask gen-lua-docs
   git diff --exit-code
   ```

4. Confirm CI passed on that exact commit. The coverage, Windows and fuzz jobs run
   nightly only; trigger them with `gh workflow run CI` and confirm they pass too.
5. Create and push the immutable tag:

   ```bash
   git switch main
   git pull --ff-only
   test -z "$(git status --porcelain)"
   git tag v<X.Y.Z>
   git push origin v<X.Y.Z>
   ```

The release workflow verifies
the immutable source commit, and runs `cargo xtask prepare-release <version>` in
each ephemeral build checkout. That command updates non-independent package
versions, internal path dependency requirements, and both lockfiles without
committing or pushing them. CI then checks the materialized package versions,
builds all artifacts from the tagged source, smoke-tests the native Linux binary
identity, and publishes SHA-256 checksums.

### Agent artifact verification

After the workflow completes:

1. Confirm the GitHub release tag and `BUILD_INFO` commit equal the intended
   source commit.
2. Verify the checksum file:

   ```bash
   sha256sum --check SHA256SUMS
   ```

3. Extract the archive for the local platform and verify the binary:

   ```bash
   ./smelt --version
   ```

The reported version must equal the immutable release tag.

## Library crates

The independently versioned crates are declared in
`[workspace.metadata.smelt.release]` in the root `Cargo.toml`. This is the
canonical allowlist used by CI and the publication workflows.

1. Update the selected crate version, its workspace dependency requirement in
   the root `Cargo.toml`, and `Cargo.lock` in a normal pull request.
2. Run formatting, clippy, tests, and `cargo publish -p <crate> --dry-run`.
3. Merge the version bump and confirm CI passed on that exact commit.
4. Tag and push the versioned commit:

   ```bash
   git switch main
   git pull --ff-only
   test -z "$(git status --porcelain)"
   git tag <crate>-v<X.Y.Z>
   git push origin <crate>-v<X.Y.Z>
   ```

When publishing multiple crates, publish dependencies before dependents.
Currently that means `smelt-style` before `smelt-ansi`, and both `smelt-style`
and `smelt-ansi` before `smelt-term`. Wait for each publish run to finish
before pushing the next tag.
