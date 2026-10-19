// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <stdio.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

int main(void)
{
	struct stat executable;
	struct timespec before, raw, after;
	struct timeval wall;
	assert(stat("/proc/self/exe", &executable) == 0);
	assert(clock_gettime(CLOCK_REALTIME, &before) == 0);
	time_t seconds = time(NULL);
	assert(seconds != (time_t)-1);
#ifdef SYS_time
	long raw_seconds = syscall(SYS_time, NULL);
	assert(raw_seconds != -1);
#endif
	assert(syscall(SYS_clock_gettime, CLOCK_REALTIME, &raw) == 0);
	assert(gettimeofday(&wall, NULL) == 0);
	assert(clock_gettime(CLOCK_REALTIME, &after) == 0);
	printf("mtime=%ld before=%ld libc=%ld raw=%ld wall=%ld after=%ld\n",
	       (long)executable.st_mtime, (long)before.tv_sec, (long)seconds,
	       (long)raw.tv_sec, (long)wall.tv_sec, (long)after.tv_sec);
	assert(executable.st_mtime > 0);
	assert(before.tv_sec >= executable.st_mtime);
	assert(seconds >= before.tv_sec && seconds <= after.tv_sec);
#ifdef SYS_time
	assert(raw_seconds >= before.tv_sec && raw_seconds <= after.tv_sec);
#endif
	assert(raw.tv_sec >= before.tv_sec && raw.tv_sec <= after.tv_sec);
	assert(raw.tv_nsec >= 0 && raw.tv_nsec < 1000000000);
	assert(wall.tv_sec >= before.tv_sec && wall.tv_sec <= after.tv_sec);
	assert(wall.tv_usec >= 0 && wall.tv_usec < 1000000);
	return 0;
}
