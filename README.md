# RMPad

This is a simple program that takes input from a remarkable tablet (only for rm2 right now) and converts it into libinput devices. This project only works on linux, and is only tested on wayland.

Features:
- Pen input (position, pressure and tilt)
- Touch input (multi-touch gestures, tapping and moving)
- Simple palm rejection (disables touch input for 0.5s if any pen input is detected)
- Optional input grab so the tablet UI doesn't see input: with `--stop-ui` or `stop_ui = true` in config, a small helper binary is uploaded to `/tmp` on the tablet and uses `EVIOCGRAB` to exclusively grab the input devices. The tablet UI (xochitl) keeps running but receives no pen/touch events. The grab is automatically released when rm-pad exits or the SSH connection drops — no reboot or manual cleanup needed.
- Works over both wifi and USB
- Very low latency (as long as your connection to the tablet is fast)
- Runs in userspace (as long as your user is allowed to create input devices)

## Installation

Either build it yourself or use the prebuilt binaries from GitHub releases.

### Building from source

You'll need Rust and C cross-compilers for ARM:

**Ubuntu/Debian:**
```bash
sudo apt install gcc-arm-linux-gnueabihf gcc-aarch64-linux-gnu
```

**Arch Linux (AUR):**
```bash
arm-linux-gnueabihf-gcc aarch64-linux-gnu-gcc
```

You can also set `ARMV7_CC` and `AARCH64_CC` environment variables to point to your cross-compilers.

Then build with:
```bash
cargo build --release
```


## Configuration

Copy the `rm-pad.toml.example` file to `~/.config/rm-pad.toml` and change the options to your preferences.

Make sure that `host` is correct (if using wifi) and `key_path` points to an authorized key or `password` contains the root password of your remarkable device.
