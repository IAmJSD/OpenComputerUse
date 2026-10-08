# Agents

## Cutting a release

1. Bump the version in `Cargo.toml` (`[workspace.package]`) and
   `CFBundleShortVersionString` in `packaging/macos/lockplugin/Info.plist`,
   then run `cargo check` so `Cargo.lock` picks it up.
2. Commit those three files with the bare version as the message (`v0.2.2`).
3. Tag it with an annotated tag and push both:
   `git tag -a v0.2.2 -m v0.2.2 && git push && git push origin v0.2.2`.
   `.github/workflows/release.yml` fails if the tag and `Cargo.toml` disagree.
4. Wait for the workflow to publish the release. Check that it has
   `OpenComputerUse.zip`, `OpenComputerUse.dmg` and the Linux and Windows
   binaries (`gh release view v0.2.2`).
5. Bump `version` and `sha256` in `Casks/opencomputeruse.rb`, using the zip
   from the release (`gh release download v0.2.2 -p OpenComputerUse.zip` then
   `shasum -a 256 OpenComputerUse.zip`), and commit that.

Installed apps pick the release up through their own updater, which only
installs builds signed by the same team, so don't publish an unsigned build.
