#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// Join the owned cgroup and report the clock immediately before target exec.
// Python startup and fixture preparation are outside the measured interval.
int main(int argc, char **argv)
{
	if (argc < 6 || strcmp(argv[4], "--") != 0) {
		fprintf(stderr, "usage: bench-exec CGROUP TIMING_FD METRICS_FILE -- PROGRAM [ARGS]\n");
		return 125;
	}
	if (strcmp(argv[1], "-") != 0) {
		char path[4096];
		int length = snprintf(path, sizeof(path), "%s/cgroup.procs", argv[1]);
		if (length < 0 || (size_t)length >= sizeof(path)) {
			fprintf(stderr, "cgroup path too long\n");
			return 125;
		}
		int fd = open(path, O_WRONLY | O_CLOEXEC);
		if (fd < 0 || dprintf(fd, "%ld\n", (long)getpid()) < 0) {
			perror("join benchmark cgroup");
			return 125;
		}
		close(fd);
	}
	char *end;
	long timing_fd = strtol(argv[2], &end, 10);
	if (*end || timing_fd < -1 || timing_fd > 1048576) {
		fprintf(stderr, "invalid timing descriptor\n");
		return 125;
	}
	/* Fork after this small native program's exec. The target's wait4 RSS
	 * then starts with this process's resident pages, not Python's earlier
	 * fixture-generation high-water mark, which survives the first exec.
	 * Background daemons have no per-command metric file and exec directly.
	 */
	if (strcmp(argv[3], "-") != 0) {
		pid_t child = fork();
		if (child < 0) {
			perror("fork benchmark target");
			return 125;
		}
		if (child > 0) {
			close((int)timing_fd);
			int status;
			struct rusage usage;
			while (wait4(child, &status, 0, &usage) < 0) {
				if (errno == EINTR) continue;
				perror("reap benchmark target");
				return 125;
			}
			FILE *metrics = fopen(argv[3], "w");
			if (!metrics) {
				perror("write target resource metrics");
				return 125;
			}
			fprintf(metrics, "{\"max_rss_bytes\":%ld,\"cpu_user_s\":%.6f,\"cpu_system_s\":%.6f}\n",
				usage.ru_maxrss * 1024,
				usage.ru_utime.tv_sec + usage.ru_utime.tv_usec / 1e6,
				usage.ru_stime.tv_sec + usage.ru_stime.tv_usec / 1e6);
			if (fclose(metrics) != 0) return 125;
			if (WIFSIGNALED(status)) {
				int signal_number = WTERMSIG(status);
				signal(signal_number, SIG_DFL);
				raise(signal_number);
				return 128 + signal_number;
			}
			return WEXITSTATUS(status);
		}
	}
	if (timing_fd >= 0) {
		struct timespec now;
		if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
			dprintf((int)timing_fd, "%" PRIu64 "\n",
				(uint64_t)now.tv_sec * 1000000000 + (uint64_t)now.tv_nsec) < 0) {
			perror("report benchmark clock");
			return 125;
		}
		close((int)timing_fd);
	}
	execv(argv[5], &argv[5]);
	perror("exec benchmark target");
	return 125;
}
