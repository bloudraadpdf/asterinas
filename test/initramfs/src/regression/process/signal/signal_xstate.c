// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

static unsigned char *copy;
static int mode;
static volatile sig_atomic_t delivered, extended;

static void receive(int number, siginfo_t *info, void *pointer)
{
	(void)info;
	assert(number == SIGUSR1);
	ucontext_t *context = pointer;
	unsigned char *fp = (void *)context->uc_mcontext.fpregs;
	uint32_t magic, size = 512, trailing;
	memcpy(&magic, fp + 464, sizeof(magic));
	extended = (context->uc_flags & 1) != 0;
	if (extended) {
		assert(magic == UINT32_C(0x46505853));
		memcpy(&size, fp + 480, sizeof(size));
		assert(size >= 576 && size <= 65532);
		memcpy(&trailing, fp + size, sizeof(trailing));
		assert(trailing == UINT32_C(0x46505845));
		size += 4;
	}
	memcpy(copy, fp, size);
	if (mode == 1) {
		context->uc_mcontext.fpregs = NULL;
	} else if (mode == 2) {
		context->uc_mcontext.fpregs = (void *)1;
	} else {
		if (mode == 3) {
			uint32_t mxcsr;
			memcpy(&mxcsr, copy + 24, sizeof(mxcsr));
			mxcsr |= UINT32_C(0x80000000);
			memcpy(copy + 24, &mxcsr, sizeof(mxcsr));
		} else if (extended && mode == 4) {
			uint64_t compact = UINT64_C(1) << 63;
			memcpy(copy + 520, &compact, sizeof(compact));
		} else if (extended && mode == 5) {
			uint64_t unsupported = UINT64_C(1) << 63;
			memcpy(copy + 512, &unsupported, sizeof(unsupported));
		}
		assert(mprotect(copy, 65536, PROT_READ) == 0);
		context->uc_mcontext.fpregs = (void *)copy;
	}
	delivered = 1;
}

static void child(void)
{
	assert(alarm(5) == 0);
	copy = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
		    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(copy != MAP_FAILED);
	struct sigaction action = { .sa_sigaction = receive,
				    .sa_flags = SA_SIGINFO };
	assert(sigemptyset(&action.sa_mask) == 0 &&
	       sigaction(SIGUSR1, &action, NULL) == 0);
	assert(raise(SIGUSR1) == 0 && delivered);
	if (mode >= 4 && !extended)
		_exit(77);
	assert(mode < 2);
	_exit(0);
}

int main(void)
{
	for (mode = 0; mode < 6; mode++) {
		pid_t pid = fork();
		assert(pid >= 0);
		if (!pid)
			child();
		int status;
		assert(waitpid(pid, &status, 0) == pid);
		if (mode < 2)
			assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
		else
			assert((WIFSIGNALED(status) &&
				WTERMSIG(status) == SIGSEGV) ||
			       (mode >= 4 && WIFEXITED(status) &&
				WEXITSTATUS(status) == 77));
	}
	puts("PASS x86 signal xstate metadata, read-only restore and malformed frame handling");
	return 0;
}
