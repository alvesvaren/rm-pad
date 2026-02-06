# RMPad

This is a simple program that takes input from a remarkable tablet (only for rm2 right now) and converts it into libinput devices. This project only works on linux, and is only tested on wayland.

Features:
- Pen input (position, pressure and tilt)
- Touch input (multi-touch gestures, tapping and moving)
- Simple palm rejection (disables touch input for 0.5s if any pen input is detected)
- Optional UI pause so the tablet UI doesn’t see input: with `--use-grab` or `no_grab = false` in config, rm-mouse runs `kill -STOP $(pidof xochitl)` over SSH before streaming, and `kill -CONT $(pidof xochitl)` when it exits. No binaries or files on the reMarkable are needed. Default is no-grab (UI sees input).
- Works over both wifi and USB
- Very low latency (as long as your connection to the tablet is fast)
- Runs in userspace (as long as your user is allowed to create input devices)

## Installation

Either build it yourself or use the prebuilt binaries. For building you'll need Rust (no cross-compilation or binaries on the tablet).


## Configuration

Copy the `rm-mouse.toml.example` file to `~/.config/rm-mouse.toml` and change the options to your preferences.

Make sure that `host` is correct (if using wifi) and `key_path` points to an authorized key or `password` contains the root password of your remarkable device.
