/*
 * rm-mouse-grabber: run on the reMarkable to grab an input device and stream
 * events to stdout. When stdout closes (SSH disconnect), we exit and release
 * the grab so the UI works again.
 *
 * If --alive-file is given, the grabber checks that the host has touched that
 * file recently; if it is older than --stale-sec seconds, the grabber exits
 * so the tablet UI becomes responsive again (e.g. after network drop).
 *
 * Build for reMarkable 2 (armv7l):
 *   arm-linux-gnueabihf-gcc -O2 -o rm-mouse-grabber grabber.c
 * Or use the Makefile.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/types.h>

/* Same as rm-mouse event.rs: 32-bit ARM input_event size */
#define INPUT_EVENT_SIZE 16
#define POLL_TIMEOUT_MS 1000

static int dev_fd = -1;
static int grab_active = 0;

static void release_grab(void) {
    if (dev_fd >= 0 && grab_active) {
        ioctl(dev_fd, EVIOCGRAB, (void *)0);
        grab_active = 0;
    }
}

static void cleanup(int fd, const char *pidfile) {
    release_grab();
    if (fd >= 0) close(fd);
    if (pidfile && pidfile[0]) unlink(pidfile);
}

static void sig_handler(int sig) {
    (void)sig;
    _exit(0);
}

/* Return 1 if alive file is stale (exists and mtime older than stale_sec), 0 otherwise. Missing file = not stale (host may not have touched yet). */
static int alive_file_stale(const char *alive_file, int stale_sec) {
    struct stat st;
    if (stat(alive_file, &st) != 0)
        return 0;
    time_t now = time(NULL);
    return (now - st.st_mtime) > (time_t)stale_sec;
}

int main(int argc, char **argv) {
    const char *device = NULL;
    const char *pidfile = NULL;
    const char *alive_file = NULL;
    int stale_sec = 10;
    int i;
    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--device") == 0 && i + 1 < argc)
            device = argv[++i];
        else if (strcmp(argv[i], "--pidfile") == 0 && i + 1 < argc)
            pidfile = argv[++i];
        else if (strcmp(argv[i], "--alive-file") == 0 && i + 1 < argc)
            alive_file = argv[++i];
        else if (strcmp(argv[i], "--stale-sec") == 0 && i + 1 < argc)
            stale_sec = atoi(argv[++i]);
    }
    if (!device || !pidfile) {
        fprintf(stderr, "Usage: %s --device /dev/input/eventN --pidfile /path/to/file.pid [--alive-file /path] [--stale-sec N]\n", argv[0]);
        return 1;
    }

    signal(SIGPIPE, sig_handler);
    signal(SIGTERM, sig_handler);
    signal(SIGINT, sig_handler);

    dev_fd = open(device, O_RDONLY);
    if (dev_fd < 0) {
        perror(device);
        return 1;
    }

    if (ioctl(dev_fd, EVIOCGRAB, (void *)1) != 0) {
        perror("EVIOCGRAB");
        close(dev_fd);
        return 1;
    }
    grab_active = 1;

    FILE *pf = fopen(pidfile, "w");
    if (pf) {
        fprintf(pf, "%d\n", (int)getpid());
        fclose(pf);
    }

    uint8_t buf[INPUT_EVENT_SIZE];

    if (!alive_file) {
        /* No self-check: blocking read only. Watchdog (if any) must kill us. */
        for (;;) {
            ssize_t n = read(dev_fd, buf, sizeof(buf));
            if (n <= 0) break;
            if (write(STDOUT_FILENO, buf, (size_t)n) != n) break;
        }
    } else {
        /* Self-check: poll with timeout, then check alive file mtime. */
        for (;;) {
            struct pollfd pfd = { .fd = dev_fd, .events = POLLIN };
            int r = poll(&pfd, 1, POLL_TIMEOUT_MS);
            if (r < 0)
                break;
            if (r == 0) {
                if (alive_file_stale(alive_file, stale_sec))
                    break;
                continue;
            }
            if (!(pfd.revents & POLLIN))
                continue;
            ssize_t n = read(dev_fd, buf, sizeof(buf));
            if (n <= 0) break;
            if (write(STDOUT_FILENO, buf, (size_t)n) != n) break;
        }
    }

    cleanup(dev_fd, pidfile);
    dev_fd = -1;
    return 0;
}
