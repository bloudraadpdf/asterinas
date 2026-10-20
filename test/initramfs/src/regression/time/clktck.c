// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static uint64_t nanoseconds(clockid_t clock)
{
	struct timespec now;
	assert(clock_gettime(clock, &now) == 0);
	return (uint64_t)now.tv_sec * 1000000000 + now.tv_nsec;
}

static uint64_t to_ticks(uint64_t nanoseconds)
{
	return nanoseconds / 10000000;
}

static void read_stat(uint64_t *user, uint64_t *system, uint64_t *start)
{
	FILE *file = fopen("/proc/self/stat", "r");
	assert(file != NULL);
	char line[4096];
	assert(fgets(line, sizeof(line), file) != NULL);
	assert(fclose(file) == 0);
	char *comm_end = strrchr(line, ')');
	assert(comm_end != NULL);
	char *field = strtok(comm_end + 1, " ");
	for (int index = 3; index <= 22; index++) {
		assert(field != NULL);
		if (index == 14)
			*user = strtoull(field, NULL, 10);
		if (index == 15)
			*system = strtoull(field, NULL, 10);
		if (index == 22)
			*start = strtoull(field, NULL, 10);
		field = strtok(NULL, " ");
	}
}

int main(void)
{
	assert(getauxval(AT_CLKTCK) == 100);
	assert(sysconf(_SC_CLK_TCK) == 100);
	uint64_t before_fork = to_ticks(nanoseconds(CLOCK_BOOTTIME));
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		uint64_t user, system, start;
		read_stat(&user, &system, &start);
		uint64_t after_fork = to_ticks(nanoseconds(CLOCK_BOOTTIME));
		fprintf(stderr, "start ticks=%llu, fork interval=%llu..%llu\n",
			(unsigned long long)start,
			(unsigned long long)before_fork,
			(unsigned long long)after_fork);
		assert(start + 1 >= before_fork && start <= after_fork + 1);

		uint64_t initial_cpu = nanoseconds(CLOCK_PROCESS_CPUTIME_ID);
		uint64_t cpu;
		do {
			for (volatile unsigned int i = 0; i < 100000; i++)
				;
			cpu = nanoseconds(CLOCK_PROCESS_CPUTIME_ID);
		} while (cpu - initial_cpu < 200000000);
		read_stat(&user, &system, &start);
		uint64_t after_stat = nanoseconds(CLOCK_PROCESS_CPUTIME_ID);
		assert(user + system + 2 >= to_ticks(cpu));
		assert(user + system <= to_ticks(after_stat) + 2);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	return 0;
}
