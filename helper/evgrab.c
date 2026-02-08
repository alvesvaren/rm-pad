/*
 * evgrab - Exclusively grab an evdev device and stream events to stdout.
 *
 * Uses EVIOCGRAB to prevent other readers (like xochitl) from seeing events.
 * When this process exits (SSH disconnect, signal, etc.), the kernel
 * automatically releases the grab and the UI resumes normal input.
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
#include <unistd.h>

static volatile sig_atomic_t should_exit = 0;

static void signal_handler(int sig) {
    (void)sig;
    should_exit = 1;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "Usage: evgrab <device>\n");
        return 1;
    }

    // Set up signal handlers for clean shutdown
    // Use signal() instead of sigaction() for simplicity
    signal(SIGTERM, signal_handler);
    signal(SIGINT, signal_handler);

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

    fprintf(stderr, "evgrab: grabbing %s (fd=%d)\n", argv[1], fd);

    char buf[4096];
    ssize_t n = 0;

    while (!should_exit) {
        // Poll both input fd and stdout to detect when stdout closes
        struct pollfd fds[2];
        fds[0].fd = fd;
        fds[0].events = POLLIN;
        fds[1].fd = STDOUT_FILENO;
        fds[1].events = POLLOUT;

        int ret = poll(fds, 2, -1);
        if (ret < 0) {
            if (errno == EINTR) {
                // Interrupted by signal, check should_exit
                continue;
            }
            fprintf(stderr, "evgrab: poll failed: %s\n", strerror(errno));
            break;
        }

        // Check if stdout is closed (SSH disconnect)
        if (fds[1].revents & (POLLERR | POLLHUP | POLLNVAL)) {
            fprintf(stderr, "evgrab: stdout closed, exiting\n");
            break;
        }

        // Check if input is ready
        if (fds[0].revents & POLLIN) {
            n = read(fd, buf, sizeof(buf));
            if (n <= 0) {
                break;
            }

            const char *p = buf;
            ssize_t remaining = n;

            while (remaining > 0) {
                ssize_t written = write(STDOUT_FILENO, p, remaining);
                if (written <= 0) {
                    // EPIPE means stdout was closed (SSH disconnect)
                    if (errno == EPIPE) {
                        fprintf(stderr, "evgrab: stdout closed, exiting\n");
                    } else {
                        fprintf(stderr, "evgrab: write failed: %s\n", strerror(errno));
                    }
                    close(fd);
                    return 1;
                }
                p += written;
                remaining -= written;
            }
        }
    }

    if (should_exit) {
        fprintf(stderr, "evgrab: received signal, exiting\n");
    } else if (n < 0) {
        if (errno == EINTR) {
            fprintf(stderr, "evgrab: read interrupted by signal\n");
        } else {
            fprintf(stderr, "evgrab: read(%s): %s\n", argv[1], strerror(errno));
        }
    } else if (n == 0) {
        fprintf(stderr, "evgrab: read(%s): EOF\n", argv[1]);
    }
    // else: stdout closed (already logged above)

    close(fd);
    return (n < 0 && !should_exit) ? 1 : 0;
}
