// Guest half of the runner's anisette front door; root LaunchDaemon, no credentials, no private entitlements.
//
// vsock 6970 -> 127.0.0.1:6970, where `apparition anisette serve` answers from the Aqua session (the only place the
// helper's attestation is valid). The host reaches vsock only through this VM's own device, so nothing listens on a
// network. A boot can still be bringing the agent up when a request arrives, so each connection waits up to 90 s for it.
//
// vsock 6971 sets the clock: the runner sends "<unix seconds>.<microseconds>\n" right after it resumes a suspended VM,
// whose clock otherwise stands still for as long as it was suspended. Anisette carries the client's time.
//
// vsock 6972 takes an update of the guest's programs from the app: the installed `apparition vm guest-apply` runs as
// root with the connection as its input and output, checks the package's signature against the VM's own update key
// (put in from recoveryOS) and every file against it, and only then replaces them (core/src/vm_update.rs).
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/vsock.h>
#include <unistd.h>

enum { ANISETTE_PORT = 6970, CLOCK_PORT = 6971, UPDATE_PORT = 6972, LOCAL_WAIT_SECONDS = 90 };
static const char APPARITION[] = "/usr/local/apparition/apparition";

static int listen_vsock(unsigned port) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_vm address;
    memset(&address, 0, sizeof(address));
    address.svm_len = sizeof(address);
    address.svm_family = AF_VSOCK;
    address.svm_cid = VMADDR_CID_ANY;
    address.svm_port = port;
    if (bind(fd, (struct sockaddr *)&address, sizeof(address)) < 0 || listen(fd, 16) < 0) { close(fd); return -1; }
    return fd;
}

static int connect_local(void) {
    for (int tries = 0; tries < LOCAL_WAIT_SECONDS * 2; tries++) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0) return -1;
        struct sockaddr_in address;
        memset(&address, 0, sizeof(address));
        address.sin_len = sizeof(address);
        address.sin_family = AF_INET;
        address.sin_port = htons(ANISETTE_PORT);
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0) return fd;
        close(fd);
        usleep(500000);
    }
    return -1;
}

static int write_all(int fd, const char *data, ssize_t size) {
    while (size > 0) {
        ssize_t n = write(fd, data, (size_t)size);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        data += n;
        size -= n;
    }
    return 0;
}

// Copies both ways until both sides have finished sending.
static void pump(int a, int b) {
    struct pollfd fds[2] = {{a, POLLIN, 0}, {b, POLLIN, 0}};
    int open = 2;
    char buffer[65536];
    while (open > 0) {
        if (poll(fds, 2, -1) < 0) { if (errno == EINTR) continue; return; }
        for (int i = 0; i < 2; i++) {
            if (fds[i].fd < 0 || !(fds[i].revents & (POLLIN | POLLHUP | POLLERR))) continue;
            int from = i ? b : a, to = i ? a : b;  // poll skips a side set to -1 once it has finished
            ssize_t n = read(from, buffer, sizeof(buffer));
            if (n < 0 && errno == EINTR) continue;
            if (n <= 0) {
                shutdown(to, SHUT_WR);
                fds[i].fd = -1;
                open--;
            } else if (write_all(to, buffer, n) < 0) {
                return;
            }
        }
    }
}

static void set_clock(int fd) {
    struct timeval timeout = {5, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    char line[64];
    ssize_t size = 0;
    while (size < (ssize_t)sizeof(line) - 1) {
        ssize_t n = read(fd, line + size, sizeof(line) - 1 - (size_t)size);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) break;
        size += n;
        if (memchr(line, '\n', (size_t)size)) break;
    }
    line[size] = 0;
    char *end = NULL;
    long long seconds = strtoll(line, &end, 10);
    long micros = (end && *end == '.') ? strtol(end + 1, NULL, 10) : 0;
    // Only a plausible time (after 2026-01-01) is taken.
    struct timeval now = {(time_t)seconds, (suseconds_t)micros};
    char reply[96];
    if (seconds <= 1767225600 || micros < 0 || micros >= 1000000) snprintf(reply, sizeof(reply), "bad time %.40s\n", line);
    else if (settimeofday(&now, NULL) != 0) snprintf(reply, sizeof(reply), "settimeofday: %s\n", strerror(errno));
    else snprintf(reply, sizeof(reply), "ok\n");
    if (strcmp(reply, "ok\n") != 0) fputs(reply, stderr);
    write_all(fd, reply, (ssize_t)strlen(reply));
}

// The update, in a child: the connection becomes its standard input and output. Signals go back to their defaults
// first, since ignored ones survive exec (an ignored SIGCHLD would break the update's own waiting for launchctl).
static void run_update(int fd, int anisette, int clock, int update) {
    pid_t child = fork();
    if (child == 0) {
        close(anisette);
        close(clock);
        close(update);
        signal(SIGCHLD, SIG_DFL);
        signal(SIGPIPE, SIG_DFL);
        if (dup2(fd, STDIN_FILENO) < 0 || dup2(fd, STDOUT_FILENO) < 0) _exit(1);
        if (fd > STDOUT_FILENO) close(fd);
        execl(APPARITION, "apparition", "vm", "guest-apply", (char *)NULL);
        const char reply[] = "error the guest has no apparition to apply the update with\n";
        write_all(STDOUT_FILENO, reply, (ssize_t)sizeof(reply) - 1);
        _exit(127);
    }
    close(fd);
}

int main(void) {
    signal(SIGPIPE, SIG_IGN);
    signal(SIGCHLD, SIG_IGN);  // children are reaped automatically
    int anisette = listen_vsock(ANISETTE_PORT), clock = listen_vsock(CLOCK_PORT), update = listen_vsock(UPDATE_PORT);
    if (anisette < 0 || clock < 0 || update < 0) { perror("vsock listen"); return 1; }
    struct pollfd fds[3] = {{anisette, POLLIN, 0}, {clock, POLLIN, 0}, {update, POLLIN, 0}};
    for (;;) {
        if (poll(fds, 3, -1) < 0) { if (errno == EINTR) continue; return 1; }
        if (fds[2].revents & POLLIN) {
            int fd = accept(update, NULL, NULL);
            if (fd >= 0) run_update(fd, anisette, clock, update);
        }
        if (fds[1].revents & POLLIN) {
            int fd = accept(clock, NULL, NULL);
            if (fd >= 0) { set_clock(fd); close(fd); }
        }
        if (fds[0].revents & POLLIN) {
            int fd = accept(anisette, NULL, NULL);
            if (fd < 0) continue;
            pid_t child = fork();
            if (child == 0) {
                close(anisette);
                close(clock);
                close(update);
                int local = connect_local();
                if (local >= 0) { pump(fd, local); close(local); }
                close(fd);
                _exit(0);
            }
            close(fd);
        }
    }
}
