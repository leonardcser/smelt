# Release

Two tracks, both tag-driven. The workflow bumps the version, builds,
publishes, and pushes the bump back to `main`. Do not pre-edit
`Cargo.toml`.

## Library crates (crates.io)

`smelt-perf`, `smelt-style`, `smelt-term`. Tag from `main`:

```bash
git tag smelt-<crate>-v<X.Y.Z>
git push origin smelt-<crate>-v<X.Y.Z>
```

If bumping multiple, tag `smelt-style` before `smelt-term` (term depends
on style) and wait for the first run to finish before pushing the next.

## Agent (binary release)

```bash
git tag v<X.Y.Z>
git push origin v<X.Y.Z>
```

Tags containing `alpha` or `beta` are marked prerelease.

## Rules

- Tag from `main`, with a clean working tree.
- Never reuse a version; if a publish fails, bump the patch and retag.
