// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void)
{
	uint64_t original, observed, mask = UINT64_C(1) << (SIGUSR2 - 1);
	assert(syscall(SYS_rt_sigprocmask, SIG_SETMASK, NULL, &original, 8) ==
	       0);
	assert(syscall(SYS_rt_sigprocmask, SIG_SETMASK, &mask, &mask, 8) == 0);
	assert(mask == original);
	assert(syscall(SYS_rt_sigprocmask, 999, NULL, &observed, 8) == 0);
	assert(observed == (UINT64_C(1) << (SIGUSR2 - 1)));
	uint64_t unchanged = UINT64_C(0x12345678);
	assert(syscall(SYS_rt_sigprocmask, SIG_SETMASK, (void *)1, &unchanged,
		       8) == -1 &&
	       errno == EFAULT);
	assert(unchanged == UINT64_C(0x12345678));
	mask = (UINT64_C(1) << (SIGUSR1 - 1)) | (UINT64_C(1) << (SIGKILL - 1)) |
	       (UINT64_C(1) << (SIGSTOP - 1));
	assert(syscall(SYS_rt_sigprocmask, SIG_SETMASK, &mask, (void *)1, 8) ==
		       -1 &&
	       errno == EFAULT);
	assert(syscall(SYS_rt_sigprocmask, 999, NULL, &observed, 8) == 0);
	assert(observed == (UINT64_C(1) << (SIGUSR1 - 1)));
	assert(syscall(SYS_rt_sigprocmask, SIG_SETMASK, &original, NULL, 8) ==
	       0);

	stack_t output, saved, before;
	memset(&output, 0x3c, sizeof(output));
	before = output;
	assert(syscall(SYS_sigaltstack, (void *)1, &output) == -1 &&
	       errno == EFAULT);
	assert(memcmp(&output, &before, sizeof(output)) == 0);
	assert(sigaltstack(NULL, &saved) == 0);
	size_t size = 128 * 1024;
	void *memory = mmap(NULL, size, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED);
	stack_t stack = { .ss_sp = memory, .ss_size = size };
	assert(syscall(SYS_sigaltstack, &stack, &stack) == 0);
	assert(stack.ss_sp == saved.ss_sp && stack.ss_size == saved.ss_size &&
	       stack.ss_flags == saved.ss_flags);
	assert(mprotect(memory, size, PROT_READ) == 0);
	stack_t disabled = { .ss_flags = SS_DISABLE };
	assert(sigaltstack(&disabled, memory) == -1 && errno == EFAULT);
	assert(sigaltstack(NULL, &output) == 0 &&
	       (output.ss_flags & SS_DISABLE));
	assert(sigaltstack(&saved, NULL) == 0 && munmap(memory, size) == 0);
	puts("PASS signal input aliases and output-fault state changes");
	return 0;
}
