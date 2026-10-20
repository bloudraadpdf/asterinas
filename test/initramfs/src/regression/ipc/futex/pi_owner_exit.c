// SPDX-License-Identifier: MPL-2.0

#include "pi_test.h"
#include <pthread.h>
#include <semaphore.h>
#include <stdio.h>

static _Atomic uint32_t word;
static atomic_int acquired, owner_died;
static sem_t ready, leave, first_acquired, first_release;

static void *owner(void *unused)
{
	(void)unused;
	assert(pi_futex(&word, FUTEX_LOCK_PI_PRIVATE, NULL) == 0);
	assert(sem_post(&ready) == 0 && sem_wait(&leave) == 0);
	return NULL;
}

static void *waiter(void *unused)
{
	assert(pi_futex(&word, FUTEX_LOCK_PI_PRIVATE, NULL) == 0);
	uint32_t value = atomic_load(&word);
	assert((value & FUTEX_TID_MASK) == (uint32_t)syscall(SYS_gettid));
	atomic_fetch_add(&acquired, 1);
	if (value & FUTEX_OWNER_DIED)
		atomic_fetch_add(&owner_died, 1);
	if ((intptr_t)unused == 0)
		assert(sem_post(&first_acquired) == 0 &&
		       sem_wait(&first_release) == 0);
	assert(pi_futex(&word, FUTEX_UNLOCK_PI_PRIVATE, NULL) == 0);
	return NULL;
}

int main(void)
{
	pthread_t holder, waiters[2];
	assert(sem_init(&ready, 0, 0) == 0 && sem_init(&leave, 0, 0) == 0);
	assert(sem_init(&first_acquired, 0, 0) == 0 &&
	       sem_init(&first_release, 0, 0) == 0);
	assert(pthread_create(&holder, NULL, owner, NULL) == 0 &&
	       sem_wait(&ready) == 0);
	assert(pthread_create(&waiters[0], NULL, waiter, NULL) == 0);
	wait_until_queued(&word, FUTEX_PRIVATE_FLAG);
	assert(sem_post(&leave) == 0 && pthread_join(holder, NULL) == 0);
	assert(sem_wait(&first_acquired) == 0);
	assert(pthread_create(&waiters[1], NULL, waiter, (void *)(intptr_t)1) ==
	       0);
	wait_until_queued(&word, FUTEX_PRIVATE_FLAG);
	assert(sem_post(&first_release) == 0);
	for (int index = 0; index < 2; index++)
		assert(pthread_join(waiters[index], NULL) == 0);
	assert(atomic_load(&acquired) == 2 && atomic_load(&owner_died) >= 1);
	assert(atomic_load(&word) == 0);
	assert(sem_destroy(&ready) == 0 && sem_destroy(&leave) == 0);
	assert(sem_destroy(&first_acquired) == 0 &&
	       sem_destroy(&first_release) == 0);
	puts("PASS PI owner exit and waiter handoff without a robust-list entry");
	return 0;
}
