# rm-pad

This is a simple program that takes input from a reMarkable tablet (only for rm2 right now) and converts it into libinput devices. This project only works on linux, and is only tested on wayland.

Features:
- Pen input (position, pressure and tilt)
- Touch input (multi-touch gestures, tapping and moving)
- Configurable palm rejection (disables touch input for a configurable grace period if any pen input is detected, default 500ms)
- Screen orientation support (portrait, landscape-right, landscape-left, inverted)
- Input grab (enabled by default): A small helper binary is uploaded to `/tmp` on the tablet and uses `EVIOCGRAB` to exclusively grab the input devices. The tablet UI (xochitl) keeps running but receives no pen/touch events. The grab is automatically released when rm-pad exits or the SSH connection drops — no reboot or manual cleanup needed. Use `--no-grab-input` to disable.
- Works over both wifi and USB
- Very low latency (as long as your connection to the tablet is fast)
- Runs in userspace (as long as your user is allowed to create input devices)
- Debug mode: `rm-pad dump touch` or `rm-pad dump pen` to dump raw input events

## Installation

Either build it yourself, use the prebuilt binaries from GitHub releases.

### Arch Linux

I haven't added this to the AUR yet, but I've created a PKGBUILD, so you can install it easily on arch:

```
git clone https://aur.archlinux.org/rm-pad.git
cd rm-pad
makepkg -si
```

The package includes a udev rule for uinput access and a systemd user service. After installation, follow the setup instructions below.

### Building from source

You'll need Rust and C cross-compilers for ARM:

**Ubuntu/Debian:**
```bash
sudo apt install gcc-arm-linux-gnueabihf gcc-aarch64-linux-gnu
```

**Arch Linux:**
```bash
sudo pacman -S arm-linux-gnueabihf-gcc aarch64-linux-gnu-gcc
```

You can also set `ARMV7_CC` and `AARCH64_CC` environment variables to point to your cross-compilers.

Then build with:
```bash
cargo build --release
```

### Setup

#### SSH Authentication

For passwordless SSH access, copy your SSH key to the tablet:

```bash
ssh-copy-id root@10.11.99.1
```

If connecting over WiFi, replace `10.11.99.1` with your tablet's IP address. The default password is usually `root` (or check your reMarkable documentation).

#### Udev Rules (required for userspace operation)

To allow rm-pad to create virtual input devices, you need to set up udev rules:

```bash
sudo cp data/50-uinput.rules /etc/udev/rules.d/
sudo groupadd -f uinput
sudo usermod -aG uinput $USER
```

Then log out and back in (or reboot), and reload udev rules:
```bash
sudo udevadm control --reload-rules
```

#### Systemd Service (optional, for automatic startup)

rm-pad can run as a persistent user service that automatically reconnects whenever the tablet becomes reachable:

1. Install the systemd service:
```bash
mkdir -p ~/.config/systemd/user
cp data/rm-pad.service ~/.config/systemd/user/
```

2. Enable lingering for your user (allows user services to run without being logged in):
```bash
loginctl enable-linger $USER
```

3. Enable and start the service:
```bash
systemctl --user enable --now rm-pad.service
```

The service runs in the background and handles connection/disconnection automatically — just plug in your tablet and it starts forwarding input. When you unplug, it detects the disconnection and waits for the next connection.

## Configuration

Config file search order:
1. `RMPAD_CONFIG` environment variable (if set)
2. `./rm-pad.toml` (current directory)
3. `~/.config/rm-pad.toml` (user config directory)

Copy the `rm-pad.toml.example` file to one of these locations (recommended: `~/.config/rm-pad.toml`) and change the options to your preferences.

### Connection settings

- **host**: reMarkable tablet IP address or hostname. Default is `10.11.99.1` (USB connection). For WiFi, use your tablet's IP address.
- **key_path**: Path to SSH private key for authentication. Defaults to your default SSH key (`~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, etc.). Only used if `password` is not set.
- **password**: Root password for SSH authentication. If set, `key_path` is ignored. **Warning**: Restrict file permissions with `chmod 600` if storing password in config file.

You can also use environment variables:
- `RMPAD_HOST`: Override host
- `RMPAD_PASSWORD`: Override password
- `RMPAD_CONFIG`: Override config file path

### Behavior options

- **touch_only**: Run touch input only (no pen)
- **pen_only**: Run pen input only (no touch)
- **grab_input**: Grab input exclusively (prevents tablet UI from seeing input, default: `true`)
- **no_palm_rejection**: Disable palm rejection
- **palm_grace_ms**: Palm rejection grace period in milliseconds (default: 500)
- **orientation**: Screen orientation - `portrait`, `landscape-right` (default), `landscape-left`, or `inverted`

All options can also be set via command-line flags. Run `rm-pad --help` for details.

## Usage

Run `rm-pad` to start forwarding input. The program will automatically reconnect if the connection drops.

For debugging, use the dump command:
```bash
rm-pad dump touch  # Dump raw touch events
rm-pad dump pen    # Dump raw pen events
```
