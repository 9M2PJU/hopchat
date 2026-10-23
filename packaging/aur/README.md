# AUR packages for hopchat

Two package bases, following Arch conventions:

| Directory     | AUR package  | Source              | When to use |
|---------------|--------------|---------------------|-------------|
| `hopchat/`    | `hopchat`    | upstream release tag `v$pkgver` | stable, once tags exist |
| `hopchat-git/`| `hopchat-git`| `main` branch (VCS) | latest commit, works today |

## Initial publish (done once per package base)

```sh
# stable
git clone ssh://aur@aur.archlinux.org/hopchat.git /tmp/aur-hopchat
cp packaging/aur/hopchat/PKGBUILD packaging/aur/hopchat/.SRCINFO /tmp/aur-hopchat/
cd /tmp/aur-hopchat && git add PKGBUILD .SRCINFO && git commit -m "init hopchat 0.1.0" && git push

# devel
git clone ssh://aur@aur.archlinux.org/hopchat-git.git /tmp/aur-hopchat-git
cp packaging/aur/hopchat-git/PKGBUILD packaging/aur/hopchat-git/.SRCINFO /tmp/aur-hopchat-git/
cd /tmp/aur-hopchat-git && git add PKGBUILD .SRCINFO && git commit -m "init hopchat-git" && git push
```

## Updating on a new upstream release

```sh
cd packaging/aur/hopchat
# bump pkgver/pkgrel in PKGBUILD, then refresh checksums:
updpkgsums          # or: makepkg -g >> PKGBUILD (replace sha256sums line)
makepkg --printsrcinfo > .SRCINFO
makepkg -si         # verify it builds (optional but recommended)
# push to AUR as above
```

For `hopchat-git`, no update is needed on release — `pkgver()` tracks the
latest commit automatically. Push only when the PKGBUILD itself changes
(dependencies, install paths).

## Notes

- `hopchat` points at `https://github.com/bahdotsh/hopchat/archive/refs/tags/v$pkgver.tar.gz`.
  The `v0.1.0` tag does not exist yet, so the stable package builds only
  after the first upstream release. Until then, use `hopchat-git`.
- Runtime deps are `gcc-libs` + `openssl` (via `native-tls`); `mdns-sd`
  discovery is pure Rust and needs no Avahi daemon.
