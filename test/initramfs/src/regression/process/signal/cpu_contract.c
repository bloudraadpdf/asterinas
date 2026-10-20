// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <cpuid.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void)
{
	cpu_set_t saved, pinned;
	assert(sched_getaffinity(0, sizeof saved, &saved) == 0);
	unsigned eax, ebx, ecx, edx;
	int rdtscp = __get_cpuid(0x80000001, &eax, &ebx, &ecx, &edx) &&
		     (edx & (1U << 27));
	int rdpid = __get_cpuid_count(7, 0, &eax, &ebx, &ecx, &edx) &&
		    (ecx & (1U << 22));
	for (int index = 0; index < CPU_SETSIZE; index++) {
		if (!CPU_ISSET(index, &saved))
			continue;
		CPU_ZERO(&pinned);
		CPU_SET(index, &pinned);
		assert(sched_setaffinity(0, sizeof pinned, &pinned) == 0);
		unsigned cpu, node, encoded;
		assert(syscall(SYS_getcpu, &cpu, &node, NULL) == 0 &&
		       cpu == (unsigned)index);
		uint16_t selector = 0x7b;
		unsigned char valid;
		__asm__ volatile("lsl %w[selector], %[encoded]; setz %[valid]"
				 : [encoded] "=r"(encoded), [valid] "=qm"(valid)
				 : [selector] "r"(selector)
				 : "cc");
		assert(valid && (encoded & 0xfff) == cpu &&
		       (encoded >> 12) == node);
		if (rdtscp) {
			unsigned low, high, auxiliary;
			__asm__ volatile("rdtscp"
					 : "=a"(low), "=d"(high),
					   "=c"(auxiliary)
					 :
					 : "memory");
			assert((auxiliary & 0xfff) == cpu &&
			       (auxiliary >> 12) == node);
		}
		if (rdpid) {
			uint64_t auxiliary;
			__asm__ volatile("rdpid %0" : "=r"(auxiliary));
			assert((auxiliary & 0xfff) == cpu &&
			       (auxiliary >> 12) == node);
		}
		assert(sched_getcpu() == index);
	}
	assert(sched_setaffinity(0, sizeof saved, &saved) == 0);
	puts("PASS getcpu, sched_getcpu, LSL and supported CPU-ID instructions agree");
	return 0;
}
