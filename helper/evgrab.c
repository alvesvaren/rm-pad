/*
 * evgrab - Grab an evdev device and stream events to stdout.
 * Exits if watchdog file /tmp/rm-pad-watchdog is older than 5 seconds.
 */

#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define WATCHDOG_FILE "/tmp/rm-pad-watchdog"
#define WATCHDOG_TIMEOUT 5

static volatile int running = 1;

static void handle_signal(int sig) {
    (void)sig;
    running = 0;
}

/* Returns 1 if watchdog is OK, 0 if stale/missing */
static int check_watchdog(void) {
    struct stat st;
    if (stat(WATCHDOG_FILE, &st) < 0)
        return 0;
    return (time(NULL) - st.st_mtime) <= WATCHDOG_TIMEOUT;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s <device>\n", argv[0]);
        return 1;
    }

    signal(SIGTERM, handle_signal);
    signal(SIGINT, handle_signal);
    signal(SIGPIPE, SIG_IGN);

    int fd = open(argv[1], O_RDONLY);
    if (fd < 0) {
        fprintf(stderr, "evgrab: open %s: %s\n", argv[1], strerror(errno));
        return 1;
    }

    if (ioctl(fd, EVIOCGRAB, 1) < 0) {
        fprintf(stderr, "evgrab: grab %s: %s\n", argv[1], strerror(errno));
        close(fd);
        return 1;
    }

    fprintf(stderr, "evgrab: grabbed %s\n", argv[1]);

    struct input_event ev;
    struct pollfd pfd = { .fd = fd, .events = POLLIN };

    while (running) {
        /* Check watchdog every poll cycle */
        if (!check_watchdog()) {
            fprintf(stderr, "evgrab: watchdog stale, exiting\n");
            break;
        }

        int ret = poll(&pfd, 1, 1000); /* 1 second timeout */
        if (ret < 0 && errno != EINTR)
            break;
        if (ret <= 0)
            continue;

        ssize_t n = read(fd, &ev, sizeof(ev));
        if (n != sizeof(ev))
            break;

        if (write(STDOUT_FILENO, &ev, sizeof(ev)) != sizeof(ev))
            break;
    }

    close(fd);
    return 0;
}
