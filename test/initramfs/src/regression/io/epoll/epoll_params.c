// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <signal.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct params {
	uint32_t busy_poll_usecs;
	uint16_t busy_poll_budget;
	uint8_t prefer_busy_poll;
	uint8_t pad;
};

#define SET_PARAMS _IOW(0x8a, 1, struct params)
#define GET_PARAMS _IOR(0x8a, 2, struct params)

static void check_params(int fd, const struct params *expected)
{
	struct params value;
	memset(&value, 0xff, sizeof(value));
	assert(ioctl(fd, GET_PARAMS, &value) == 0);
	assert(memcmp(&value, expected, sizeof(value)) == 0);
}

static volatile sig_atomic_t interrupted;

static void interrupt_wait(int signal)
{
	(void)signal;
	interrupted = 1;
}

static uint64_t monotonic_ms(void)
{
	struct timespec now;
	assert(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
	return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}

static void seed_target(int epoll, int sender, int receiver,
			const struct sockaddr_in *address)
{
	char payload = 'x', received;
	assert(sendto(sender, &payload, 1, 0, (const struct sockaddr *)address,
		      sizeof(*address)) == 1);
	struct epoll_event event;
	assert(epoll_wait(epoll, &event, 1, 1000) == 1);
	assert(event.data.fd == receiver && (event.events & EPOLLIN));
	assert(recv(receiver, &received, 1, 0) == 1 && received == payload);
}

static void check_network_waits(void)
{
	int epoll = epoll_create1(0);
	int receiver = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
	int sender = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
	assert(epoll >= 0 && receiver >= 0 && sender >= 0);
	struct sockaddr_in address = {
		.sin_family = AF_INET,
		.sin_addr.s_addr = htonl(INADDR_LOOPBACK),
	};
	assert(bind(receiver, (struct sockaddr *)&address, sizeof(address)) ==
	       0);
	socklen_t length = sizeof(address);
	assert(getsockname(receiver, (struct sockaddr *)&address, &length) ==
	       0);
	struct epoll_event event = { .events = EPOLLIN, .data.fd = receiver };
	assert(epoll_ctl(epoll, EPOLL_CTL_ADD, receiver, &event) == 0);
	struct params params = { 0, 1, 1, 0 };
	assert(ioctl(epoll, SET_PARAMS, &params) == 0);
	seed_target(epoll, sender, receiver, &address);
	/* With zero usecs, prefer_busy_poll still enables a single bounded poll. */
	assert(epoll_wait(epoll, &event, 1, 1) == 0);

	params.busy_poll_usecs = 500000;
	assert(ioctl(epoll, SET_PARAMS, &params) == 0);
	seed_target(epoll, sender, receiver, &address);
	uint64_t start = monotonic_ms();
	assert(epoll_wait(epoll, &event, 1, 20) == 0);
	/* A 500 ms busy-poll setting must not replace the overall 20 ms wait. */
	assert(monotonic_ms() - start < 300);

	seed_target(epoll, sender, receiver, &address);
	struct sigaction action = { .sa_handler = interrupt_wait }, previous;
	assert(sigemptyset(&action.sa_mask) == 0);
	assert(sigaction(SIGALRM, &action, &previous) == 0);
	struct itimerval timer = { .it_value.tv_usec = 20000 };
	interrupted = 0;
	assert(setitimer(ITIMER_REAL, &timer, NULL) == 0);
	start = monotonic_ms();
	errno = 0;
	int signal_result = epoll_wait(epoll, &event, 1, 1000);
	int signal_errno = errno;
	uint64_t signal_elapsed = monotonic_ms() - start;
	if (signal_result != -1 || signal_errno != EINTR || !interrupted ||
	    signal_elapsed >= 300)
		fprintf(stderr,
			"epoll signal wait: result=%d errno=%d handler=%d elapsed_ms=%" PRIu64
			"\n",
			signal_result, signal_errno, interrupted,
			signal_elapsed);
	assert(signal_result == -1 && signal_errno == EINTR);
	assert(interrupted && signal_elapsed < 300);
	memset(&timer, 0, sizeof(timer));
	assert(setitimer(ITIMER_REAL, &timer, NULL) == 0);
	assert(sigaction(SIGALRM, &previous, NULL) == 0);
	assert(close(sender) == 0);
	assert(close(receiver) == 0);
	assert(close(epoll) == 0);
}

int main(void)
{
	int fd = epoll_create1(0);
	int other = epoll_create1(0);
	assert(fd >= 0 && other >= 0);
	struct params zero = { 0 };
	check_params(fd, &zero);
	struct params value = { 40, 8, 1, 0 };
	assert(ioctl(fd, SET_PARAMS, &value) == 0);
	int duplicate = dup(fd);
	assert(duplicate >= 0);
	check_params(duplicate, &value);
	check_params(other, &zero);

	struct params invalid[] = {
		{ 40, 8, 1, 1 },
		{ UINT32_MAX, 8, 1, 0 },
		{ 40, 8, 2, 0 },
	};
	for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++) {
		errno = 0;
		assert(ioctl(fd, SET_PARAMS, &invalid[i]) == -1 &&
		       errno == EINVAL);
		check_params(fd, &value);
	}
	errno = 0;
	assert(ioctl(fd, SET_PARAMS, (void *)1) == -1 && errno == EFAULT);
	errno = 0;
	assert(ioctl(fd, GET_PARAMS, (void *)1) == -1 && errno == EFAULT);
	check_params(fd, &value);

	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		if (getuid() == 0)
			assert(setresuid(60002, 60002, 60002) == 0);
		struct params privileged = { 40, 65, 1, 0 };
		errno = 0;
		assert(ioctl(fd, SET_PARAMS, &privileged) == -1 &&
		       errno == EPERM);
		check_params(fd, &value);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	assert(ioctl(fd, SET_PARAMS, &zero) == 0);
	check_params(duplicate, &zero);
	assert(close(duplicate) == 0);
	assert(close(other) == 0);
	assert(close(fd) == 0);
	check_network_waits();
	return 0;
}
