/*
 * evgrab - Exclusively grab an evdev device and stream events to stdout.
 *
 * Uses EVIOCGRAB to prevent other readers (like xochitl) from seeing events.
 * When this process exits, the kernel automatically releases the grab and
 * the UI resumes normal input.
 *
 * WATCHDOG: This program checks a watchdog file's modification time.
 * If the file hasn't been touched recently, we assume the host is dead
 * and exit immediately to release the grab.
 *
 * Cross-compiled for ARM and embedded in the rm-pad host binary.
 * Uploaded to /tmp on the reMarkable at runtime.
 */

#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t should_exit = 0;

static void signal_handler(int sig) {
    (void)sig;
    should_exit = 1;
}

/* Watchdog file path — host touches this periodically */
#define WATCHDOG_FILE "/tmp/rm-pad-watchdog"

/* If watchdog file is older than this, exit */
#define WATCHDOG_TIMEOUT_SECS 5

/* How often to check the watchdog (also used for poll timeout) */
#define CHECK_INTERVAL_MS 1000

static time_t get_file_mtime(const char *path)
{
    struct stat st;
    if (stat(path, &st) < 0)
        return 0;
    return st.st_mtime;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "Usage: evgrab <device>\n");
        return 1;
    }

    signal(SIGTERM, signal_handler);
    signal(SIGINT, signal_handler);
    signal(SIGHUP, signal_handler);
    signal(SIGPIPE, SIG_IGN);

    int fd = open(argv[1], O_RDONLY);
    if (fd < 0) {
        fprintf(stderr, "evgrab: open(%s): %s\n", argv[1], strerror(errno));
        return 1;
    }

    if (ioctl(fd, EVIOCGRAB, 1) != 0) {
        fprintf(stderr, "evgrab: EVIOCGRAB(%s): %s\n", argv[1], strerror(errno));
        close(fd);
        return 1;
    }

    fprintf(stderr, "evgrab: grabbing %s (fd=%d), watchdog=%s\n",
            argv[1], fd, WATCHDOG_FILE);

    char buf[4096];
    int exit_status = 0;

    while (!should_exit) {
        /* Check watchdog file */
        time_t now = time(NULL);
        time_t mtime = get_file_mtime(WATCHDOG_FILE);
        
        if (mtime == 0) {
            fprintf(stderr, "evgrab: watchdog file missing, exiting\n");
            exit_status = 1;
            break;
        }
        
        if (now - mtime > WATCHDOG_TIMEOUT_SECS) {
            fprintf(stderr, "evgrab: watchdog stale (%ld seconds old), exiting\n",
                    (long)(now - mtime));
            exit_status = 1;
            break;
        }

        struct pollfd pfd = { .fd = fd, .events = POLLIN };
        int ret = poll(&pfd, 1, CHECK_INTERVAL_MS);
        
        if (ret < 0) {
            if (errno == EINTR)
                continue;
            fprintf(stderr, "evgrab: poll: %s\n", strerror(errno));
            exit_status = 1;
            break;
        }

        if (ret == 0)
            continue;  /* Timeout, loop back to check watchdog */

        if (pfd.revents & (POLLERR | POLLHUP)) {
            fprintf(stderr, "evgrab: input device error\n");
            exit_status = 1;
            break;
        }

        if (!(pfd.revents & POLLIN))
            continue;

        ssize_t n = read(fd, buf, sizeof(buf));
        if (n < 0) {
            if (errno == EINTR)
                continue;
            fprintf(stderr, "evgrab: read(%s): %s\n", argv[1], strerror(errno));
            exit_status = 1;
            break;
        }
        if (n == 0) {
            fprintf(stderr, "evgrab: read(%s): EOF\n", argv[1]);
            break;
        }

        /* Write events to stdout */
        const char *p = buf;
        ssize_t remaining = n;
        while (remaining > 0) {
            ssize_t w = write(STDOUT_FILENO, p, remaining);
            if (w <= 0) {
                if (errno == EPIPE)
                    fprintf(stderr, "evgrab: stdout closed\n");
                else
                    fprintf(stderr, "evgrab: write: %s\n", strerror(errno));
                close(fd);
                return 1;
            }
            p += w;
            remaining -= w;
        }
    }

    if (should_exit)
        fprintf(stderr, "evgrab: received signal, exiting\n");

    close(fd);
    return exit_status;
}
