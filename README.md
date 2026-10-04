# blongo

## Releases

Pushing a `v*` tag builds Blongo and publishes it as a GitHub Release
(`.github/workflows/release.yml`), with `SHA256SUMS.txt`:

- macOS (Apple Silicon only): `Blongo-<tag>-macos-arm64.dmg`. Open it
  and drag Blongo to Applications. `blongo-serve` is inside the app at
  `Blongo.app/Contents/MacOS/blongo-serve`.
- Linux x86_64 and Windows x86_64: an archive with `blongo` and
  `blongo-serve`.

A tag with a hyphen, such as `v0.1.0-rc.1`, becomes a pre-release.

```sh
git tag v0.1.0
git push origin v0.1.0
```

From a phone or the browser: Releases → Draft a new release → choose a new
tag such as `v0.1.0` on `main` → Publish. The files are attached to that
release when the builds finish.

The macOS app is signed and notarized when the `release` environment has the
Apple signing secrets; that job waits for the owner's approval on each
release (docs/macos-signing.md). Until then the app is ad-hoc signed and
macOS asks for confirmation on first launch (System Settings → Privacy &
Security → Open Anyway). Linux and Windows binaries are not signed.
