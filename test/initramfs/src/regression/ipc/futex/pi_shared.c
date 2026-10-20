// SPDX-License-Identifier: MPL-2.0

#include "pi_test.h"
#include <stdio.h>
#include <sys/shm.h>
#include <sys/wait.h>

int main(void)
{
	int id = shmget(IPC_PRIVATE, 4096, 0600);
	assert(id >= 0);
	_Atomic uint32_t *first = shmat(id, NULL, 0);
	_Atomic uint32_t *second = shmat(id, NULL, 0);
	assert(first != (void *)-1 && second != (void *)-1 && first != second);
	assert(pi_futex(first, FUTEX_LOCK_PI, NULL) == 0);
	int ready[2];
	assert(pipe(ready) == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		assert(close(ready[0]) == 0 && write(ready[1], "x", 1) == 1);
		assert(pi_futex(second, FUTEX_LOCK_PI, NULL) == 0);
		assert((atomic_load(second) & FUTEX_TID_MASK) ==
		       (uint32_t)syscall(SYS_gettid));
		assert(pi_futex(first, FUTEX_UNLOCK_PI, NULL) == 0);
		_exit(0);
	}
	char byte;
	assert(close(ready[1]) == 0 && read(ready[0], &byte, 1) == 1 &&
	       close(ready[0]) == 0);
	wait_until_queued(first, 0);
	_Atomic uint32_t other = 0;
	assert(syscall(SYS_futex, first, FUTEX_REQUEUE, 0, 1, &other, 0) ==
		       -1 &&
	       errno == EINVAL);
	assert(syscall(SYS_futex, first, FUTEX_WAKE_OP, 1, 0, &other,
		       FUTEX_OP(FUTEX_OP_SET, 7, FUTEX_OP_CMP_EQ, 0)) == -1 &&
	       errno == EINVAL);
	assert(atomic_load(&other) == 7);
	assert(pi_futex(second, FUTEX_UNLOCK_PI, NULL) == 0);
	int status;
	assert(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
	       WEXITSTATUS(status) == 0);
	assert(atomic_load(first) == 0 && atomic_load(second) == 0);
	assert(shmctl(id, IPC_RMID, NULL) == 0);
	assert(shmdt(first) == 0 && shmdt(second) == 0);
	puts("PASS shared PI identity, cross-process ownership and mixed wake rejection");
	return 0;
}
