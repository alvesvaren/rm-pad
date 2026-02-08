# Maintainer: Your Name <your.email@example.com>
pkgname=rm-pad
pkgver=0.1.3
pkgrel=1
pkgdesc="Forward reMarkable tablet input to your computer as libinput devices"
arch=('x86_64')
url="https://github.com/alvesvaren/rm-pad"
license=('MIT' 'Apache-2.0')
depends=('gcc-libs')
makedepends=('rust' 'cargo' 'aarch64-linux-gnu-gcc' 'aarch64-linux-gnu-binutils' 'arm-none-linux-gnueabihf-toolchain-bin')
source=("git+${url}.git#tag=v${pkgver}")
sha256sums=('SKIP')

build() {
  # Build from source - cargo build will run build.rs which cross-compiles ARM helper binaries
  # Set CARGO_TARGET_DIR to avoid picking up parent Cargo.toml
  export CARGO_TARGET_DIR="$srcdir/target"
  cd "$srcdir/rm-pad"
  cargo build --release --locked
}

package() {
  cd "$srcdir/rm-pad"

  install -Dm755 "$srcdir/target/release/rm-pad" "$pkgdir/usr/bin/rm-pad"

  install -Dm644 data/50-uinput.rules "$pkgdir/usr/lib/udev/rules.d/50-uinput.rules"
  install -Dm644 data/70-rm-pad.rules "$pkgdir/usr/lib/udev/rules.d/70-rm-pad.rules"

  install -Dm644 data/rm-pad@.service "$pkgdir/usr/lib/systemd/user/rm-pad@.service"

  install -Dm644 rm-pad.toml.example "$pkgdir/usr/share/rm-pad/rm-pad.toml.example"

  install -Dm644 README.md "$pkgdir/usr/share/doc/rm-pad/README.md"
}
