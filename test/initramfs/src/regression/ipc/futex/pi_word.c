// SPDX-License-Identifier: MPL-2.0

#include "pi_test.h"
#include <stdio.h>
#include <sys/mman.h>
#include <sys/wait.h>

int main(void)
{
	int policy = sched_getscheduler(0);
	struct sched_param original, parameter = { .sched_priority = 17 };
	assert(policy >= 0 && sched_getparam(0, &original) == 0);
	assert(sched_setscheduler(0, SCHED_RR, &parameter) == 0);
	assert(sched_getparam(0, &parameter) == 0 &&
	       parameter.sched_priority == 17);
	parameter.sched_priority = 23;
	assert(sched_setparam(0, &parameter) == 0);
	assert(sched_getparam(0, &parameter) == 0 &&
	       parameter.sched_priority == 23);
	assert(sched_getscheduler(0) == SCHED_RR);
	assert(sched_setscheduler(0, policy, &original) == 0);
	_Atomic uint32_t word = FUTEX_TID_MASK;
	uint32_t tid = (uint32_t)syscall(SYS_gettid);
	assert(pi_futex(&word, FUTEX_TRYLOCK_PI_PRIVATE, NULL) == -1 &&
	       errno == ESRCH);
	atomic_store(&word, 0);
	int commands[] = { FUTEX_LOCK_PI_PRIVATE, FUTEX_TRYLOCK_PI_PRIVATE,
			   FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG,
			   FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG |
				   FUTEX_CLOCK_REALTIME };
	struct timespec expired = { 0, 0 };
	for (unsigned int index = 0;
	     index < sizeof(commands) / sizeof(commands[0]); index++) {
		assert(pi_futex(&word, commands[index], &expired) == 0);
		assert(atomic_load(&word) == tid);
		assert(pi_futex(&word, commands[index], NULL) == -1 &&
		       errno == EDEADLK);
		assert(pi_futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL) == 0);
		assert(atomic_load(&word) == 0);
	}
	assert(pi_futex(&word, FUTEX_LOCK_PI_PRIVATE | FUTEX_CLOCK_REALTIME,
			NULL) == -1 &&
	       errno == ENOSYS);
	struct timespec invalid = { .tv_nsec = 1000000000 };
	assert(pi_futex(&word, FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG, &invalid) ==
		       -1 &&
	       errno == EINVAL);
	assert(pi_futex((char *)&word + 1, FUTEX_TRYLOCK_PI_PRIVATE, NULL) ==
		       -1 &&
	       errno == EINVAL);
	assert(pi_futex((void *)4096, FUTEX_TRYLOCK_PI_PRIVATE, NULL) == -1 &&
	       errno == EFAULT);
	assert(pi_futex(&word, FUTEX_TRYLOCK_PI_PRIVATE, (void *)1) == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		assert(pi_futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL) == -1 &&
		       errno == EPERM);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
	       WEXITSTATUS(status) == 0);
	assert(pi_futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL) == 0);
	atomic_store(&word, FUTEX_OWNER_DIED);
	assert(pi_futex(&word, FUTEX_TRYLOCK_PI_PRIVATE, NULL) == 0);
	assert(atomic_load(&word) == (FUTEX_OWNER_DIED | tid));
	assert(pi_futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL) == 0);
	size_t page = (size_t)sysconf(_SC_PAGESIZE);
	void *readonly =
		mmap(NULL, page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(readonly != MAP_FAILED);
	assert(pi_futex(readonly, FUTEX_TRYLOCK_PI_PRIVATE, NULL) == -1 &&
	       errno == EFAULT);
	assert(munmap(readonly, page) == 0);
	puts("PASS PI word ownership, clocks, argument validation and write faults");
	return 0;
}
