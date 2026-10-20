// SPDX-License-Identifier: MPL-2.0

#ifndef PI_TEST_H
#define PI_TEST_H

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int pi_futex(void *word, int command, const struct timespec *timeout)
{
	return (int)syscall(SYS_futex, word, command, 0, timeout, NULL, 0);
}

static inline void wait_until_queued(_Atomic uint32_t *word, int flags)
{
	struct timespec start, current;
	assert(clock_gettime(CLOCK_MONOTONIC, &start) == 0);
	for (;;) {
		int result = (int)syscall(SYS_futex, word, FUTEX_WAKE | flags,
					  1, NULL, NULL, 0);
		if (result == -1 && errno == EINVAL)
			return;
		assert(result == 0);
		assert(clock_gettime(CLOCK_MONOTONIC, &current) == 0);
		assert(current.tv_sec - start.tv_sec < 5);
		sched_yield();
	}
}

#endif
