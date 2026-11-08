# Packaging

The release workflow (`.github/workflows/release.yml`) builds the archives, the universal macOS binary, `install.sh` and `SHA256SUMS` into a GitHub release. What lives here is published by hand after it, from that release's files:

| Path | What |
|---|---|
| `aur/PKGBUILD` | The AUR package `lsync-bin`: the static musl binary for x86_64 and aarch64, into `/usr/bin/lsync`. |
| `homebrew/lsync.rb` | The tap formula `lsync`: the universal binary on macOS, the musl binaries on Linux, and a `service do` block for `brew services`. |
| `docker/Dockerfile`, `docker/build.sh` | The image `ghcr.io/nytafar/lsync`: `FROM scratch`, the static binary, `lsync serve` as a system service run by uid 10001, with `/config` and `/state` as volumes. |
| `fill.sh` | Fills the version and the checksums into the PKGBUILD and the formula. |

The PKGBUILD and the formula are templates: `@VERSION@`, `@PKGVER@` (the version with `-` as `_`) and `@SHA256_<target>@` stand for the release's version and the SHA-256 of `lsync-<version>-<target>.tar.gz`. `fill.sh` takes them from the release's `SHA256SUMS`, fails on any it cannot fill, and writes the filled files elsewhere; the templates stay as they are.

## At a release

After the release workflow published `vX.Y.Z` (here `0.4.0`):

```sh
v=0.4.0
rel=$(mktemp -d)
gh release download "v$v" -D "$rel" -p SHA256SUMS -p "lsync-$v-*-unknown-linux-musl.tar.gz"
(cd "$rel" && sha256sum -c --ignore-missing SHA256SUMS)
pkg/fill.sh "$v" "$rel/SHA256SUMS" "$rel/pkg"     # -> $rel/pkg/aur/PKGBUILD, $rel/pkg/homebrew/lsync.rb
```

**AUR** (an AUR account with your SSH key; the first push creates the package):

```sh
git clone ssh://aur@aur.archlinux.org/lsync-bin.git ~/aur/lsync-bin
cp "$rel/pkg/aur/PKGBUILD" ~/aur/lsync-bin/
cd ~/aur/lsync-bin
makepkg --printsrcinfo > .SRCINFO
makepkg -f && namcap PKGBUILD lsync-bin-*.pkg.tar.zst    # downloads and checks the archive
git add PKGBUILD .SRCINFO && git commit -m "lsync-bin $v" && git push
```

**Homebrew tap** (a public repo `nytafar/homebrew-tap`; `brew install nytafar/tap/lsync` finds it by that name):

```sh
git clone git@github.com:nytafar/homebrew-tap.git ~/homebrew-tap
mkdir -p ~/homebrew-tap/Formula && cp "$rel/pkg/homebrew/lsync.rb" ~/homebrew-tap/Formula/
cd ~/homebrew-tap && git add Formula/lsync.rb && git commit -m "lsync $v" && git push
brew update && brew install nytafar/tap/lsync && brew test lsync && brew audit --strict nytafar/tap/lsync
```

**Docker image** (a token with `write:packages`; `docker buildx` with a builder that can build for both platforms, e.g. `docker buildx create --use`):

```sh
gh auth token | docker login ghcr.io -u nytafar --password-stdin    # or a PAT with write:packages
pkg/docker/build.sh "$v" "$rel" --load --platform linux/amd64 -t lsync:test   # try it first
docker run --rm lsync:test --version
pkg/docker/build.sh "$v" "$rel" --push --platform linux/amd64,linux/arm64 \
  -t "ghcr.io/nytafar/lsync:$v" -t ghcr.io/nytafar/lsync:latest
```

The first push creates a private package: make it public under the package's settings on GitHub. For a pre-release, leave out the `latest` tag.

## The formula's service

`brew services start lsync` runs `lsync serve` with the semantics of the LaunchAgent that `lsync service install` writes: started at load, restarted only after an unsuccessful exit (`KeepAlive` `SuccessfulExit` false; on Linux `Restart=on-failure`), a 10 s throttle, and `ProcessType` `Standard`, never `Background`. Its label is Homebrew's (`homebrew.mxcl.lsync`), not `no.nyta.lsync`, and its log is `$(brew --prefix)/var/log/lsync.log`, so `lsync service status` and `lsync service logs` do not see it. Use one of the two; the second service to start is refused by the control socket's lock.

## The Docker image

It runs `lsync serve` as `lsync --system` (`LSYNC_SYSTEM=true`) with the system service's folders moved: the config in `/config` (`CONFIGURATION_DIRECTORY`), each vault's index in `/state/<vault>` (`STATE_DIRECTORY`), the control socket in `/state/.control` (`LSYNC_RUNTIME_DIR`). Vault folders are mounted where the config says, by convention under `/vaults`. Logs go to stderr without colour. SIGTERM stops it cleanly. See the README's Docker section for running it.
