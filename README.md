# blongo

## Releases

Pushing a `v*` tag builds `blongo` (the app) and `blongo-serve` (the remote
server) for Linux x86_64, macOS arm64 / x86_64 and Windows x86_64, and
publishes them with `SHA256SUMS.txt` as a GitHub Release
(`.github/workflows/release.yml`). A tag with a hyphen, such as
`v0.1.0-rc.1`, becomes a pre-release.

```sh
git tag v0.1.0
git push origin v0.1.0
```

From a phone or the browser: Releases → Draft a new release → choose a new
tag such as `v0.1.0` on `main` → Publish. The binaries are attached to that
release when the builds finish.

The binaries are not code-signed: macOS Gatekeeper and Windows SmartScreen
warn on first launch.
