# rm-mouse-grabber

Small C program that runs **on the reMarkable tablet**. It grabs the pen or touch input device (EVIOCGRAB) so the tablet UI (xochitl) does not receive events, then streams those events to stdout. The host reads this stream over SSH.

The grabber exits (and releases the grab) when:
- The SSH pipe closes (host disconnect), or
- The host stops touching the alive file for more than `--stale-sec` seconds (e.g. network drop or host crash). The host touches the file every 2 s, so after ~10 s without a touch the grabber exits and the UI works again.

## Build (cross-compile for reMarkable)

On the tablet run **`uname -m`** to see the architecture. Then on your host install the matching cross-compiler and build:

| `uname -m` | Device        | Cross-compiler              | Make |
|------------|---------------|-----------------------------|------|
| armv7l     | reMarkable 2 (and rM1) | `arm-linux-gnueabihf-gcc` | `make` (default) |
| aarch64    | 64-bit ARM    | `aarch64-linux-gnu-gcc`     | `make ARCH=aarch64` |

```bash
cd grabber
make          # for armv7l (reMarkable 2)
# or
make ARCH=aarch64   # for aarch64
```

This produces `rm-mouse-grabber`. Copy it to the tablet:

```bash
scp rm-mouse-grabber root@10.11.99.1:/home/root/
ssh root@10.11.99.1 chmod +x /home/root/rm-mouse-grabber
```

If your tablet IP differs, set `HOST` in `src/config.rs` or use the same host as for rm-mouse.

**"cannot execute binary file: Exec format error"** means the binary was built for the wrong CPU. On the tablet run `uname -m`, then rebuild on your host with the correct cross-compiler (see table above). Do not copy a binary built with plain `gcc` on an x86 PC—it will not run on ARM.

**Still getting an x86 binary?** The Makefile uses `CROSS_CC`, not `CC`, so an environment variable `CC=gcc` won’t override it. If your ARM toolchain has a different name (e.g. Linaro: `arm-linux-gnueabihf-gcc-7.5`), run:
`make CROSS_CC=arm-linux-gnueabihf-gcc-7.5`
(or whatever `which arm-linux-gnueabihf-gcc*` shows). After a successful build, `file rm-mouse-grabber` should say "ARM" (not "x86" or "ELF 64-bit").

## Optional: external watchdog

The grabber checks the alive file itself and exits after ~10 s if the host disappears, so the tablet UI is restored without any extra setup. You can optionally install the external watchdog script as a safety net (e.g. if the grabber crashes without exiting).

```bash
scp data/rm-mouse-watchdog.sh root@10.11.99.1:/usr/bin/rm-mouse-watchdog
ssh root@10.11.99.1 chmod +x /usr/bin/rm-mouse-watchdog
# Cron (every minute): crontab -e then add: * * * * * /usr/bin/rm-mouse-watchdog
# Or systemd timer (every 10 s): copy data/rm-mouse-watchdog.service and .timer to /etc/systemd/system/, then systemctl enable --now rm-mouse-watchdog.timer
```


## Usage

You do **not** run the grabber by hand. When you run `rm-mouse` on your computer (without `--no-grab`), it will SSH to the tablet and start the grabber for pen and/or touch. The grabber path is set in `config.rs` (`GRABBER_PATH`, default `/home/root/rm-mouse-grabber`).

Use `--no-grab` to disable grabbing and use plain `cat` (tablet UI will still receive input).
