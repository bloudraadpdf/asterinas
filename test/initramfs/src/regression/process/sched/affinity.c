// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ROUNDS 64

static atomic_int worker_tid;
static atomic_int ready;
static atomic_int command;
static atomic_int acknowledged;
static atomic_int expected_cpu;
static atomic_int release_sleep;

static void publish(atomic_int *word, int value)
{
	atomic_store_explicit(word, value, memory_order_release);
	assert(syscall(SYS_futex, word, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL,
		       0) >= 0);
}

static void wait_for(atomic_int *word, int value)
{
	for (;;) {
		int observed = atomic_load_explicit(word, memory_order_acquire);
		if (observed >= value)
			return;
		long result = syscall(SYS_futex, word, FUTEX_WAIT_PRIVATE,
				      observed, NULL, NULL, 0);
		assert(result == 0 || errno == EAGAIN || errno == EINTR);
	}
}

static void check_cpu(int expected)
{
	unsigned int actual;
	assert(syscall(SYS_getcpu, &actual, NULL, NULL) == 0);
	assert(actual == (unsigned int)expected);
	assert(sched_getcpu() == expected);
}

static void pin(pid_t tid, int cpu)
{
	cpu_set_t one;
	CPU_ZERO(&one);
	CPU_SET(cpu, &one);
	assert(sched_setaffinity(tid, sizeof(one), &one) == 0);
}

static void *worker(void *unused)
{
	(void)unused;
	publish(&worker_tid, (int)syscall(SYS_gettid));
	for (int round = 1; round <= ROUNDS; round++) {
		publish(&ready, round);
		/* Keep the target runnable while another thread changes its mask. */
		while (atomic_load_explicit(&command, memory_order_acquire) !=
		       round)
			atomic_signal_fence(memory_order_seq_cst);
		check_cpu(atomic_load_explicit(&expected_cpu,
					       memory_order_relaxed));
		publish(&acknowledged, round);
	}
	publish(&ready, ROUNDS + 1);
	wait_for(&release_sleep, 1);
	check_cpu(atomic_load_explicit(&expected_cpu, memory_order_relaxed));
	return NULL;
}

int main(void)
{
	alarm(30);
	cpu_set_t allowed;
	assert(sched_getaffinity(0, sizeof(allowed), &allowed) == 0);
	int cpus[CPU_SETSIZE];
	int count = 0;
	for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) {
		if (!CPU_ISSET(cpu, &allowed))
			continue;
		cpus[count++] = cpu;
		pin(0, cpu);
		check_cpu(cpu);
	}
	assert(count > 0);
	assert(sched_setaffinity(0, sizeof(allowed), &allowed) == 0);

	cpu_set_t empty;
	CPU_ZERO(&empty);
	errno = 0;
	assert(sched_setaffinity(0, sizeof(empty), &empty) == -1);
	assert(errno == EINVAL);
	errno = 0;
	assert(sched_setaffinity(INT_MAX, sizeof(allowed), &allowed) == -1);
	assert(errno == ESRCH);

	pthread_t thread;
	assert(pthread_create(&thread, NULL, worker, NULL) == 0);
	wait_for(&worker_tid, 1);
	pid_t tid = atomic_load_explicit(&worker_tid, memory_order_acquire);
	for (int round = 1; round <= ROUNDS; round++) {
		wait_for(&ready, round);
		int cpu = cpus[(round - 1) % count];
		pin(tid, cpu);
		atomic_store_explicit(&expected_cpu, cpu, memory_order_relaxed);
		publish(&command, round);
		wait_for(&acknowledged, round);
	}
	wait_for(&ready, ROUNDS + 1);
	pin(tid, cpus[0]);
	atomic_store_explicit(&expected_cpu, cpus[0], memory_order_relaxed);
	publish(&release_sleep, 1);
	assert(pthread_join(thread, NULL) == 0);
	assert(sched_setaffinity(0, sizeof(allowed), &allowed) == 0);
	alarm(0);
	puts("PASS self and concurrent target affinity with raw and libc CPU IDs");
	return 0;
}
