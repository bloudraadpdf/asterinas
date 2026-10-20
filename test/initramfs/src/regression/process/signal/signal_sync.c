// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static size_t page;
static volatile sig_atomic_t repaired;

static void repair(int number, siginfo_t *info, void *context)
{
	(void)context;
	assert(number == SIGSEGV && info->si_code == SEGV_ACCERR);
	assert(mprotect(info->si_addr, page, PROT_READ | PROT_WRITE) == 0);
	repaired = 1;
}

static void fault(int mode)
{
	assert(alarm(5) == 0);
	if (mode == 1) {
		sigset_t blocked;
		assert(sigemptyset(&blocked) == 0 &&
		       sigaddset(&blocked, SIGSEGV) == 0);
		assert(sigprocmask(SIG_BLOCK, &blocked, NULL) == 0);
	} else if (mode == 2) {
		assert(signal(SIGSEGV, SIG_IGN) != SIG_ERR);
	} else if (mode == 3) {
		struct sigaction action = { .sa_sigaction = repair,
					    .sa_flags = SA_SIGINFO };
		assert(sigemptyset(&action.sa_mask) == 0 &&
		       sigaction(SIGSEGV, &action, NULL) == 0);
	}
	void *memory =
		mmap(NULL, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED);
	*(volatile char *)memory = 42;
	assert(mode == 3 && repaired && *(volatile char *)memory == 42);
	assert(munmap(memory, page) == 0);
	_exit(0);
}

int main(void)
{
	page = (size_t)sysconf(_SC_PAGESIZE);
	for (int mode = 0; mode < 4; mode++) {
		pid_t child = fork();
		assert(child >= 0);
		if (!child)
			fault(mode);
		int status;
		assert(waitpid(child, &status, 0) == child);
		if (mode == 3)
			assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
		else
			assert(WIFSIGNALED(status) &&
			       WTERMSIG(status) == SIGSEGV);
	}
	puts("PASS synchronous fault delivery and protection-fault repair");
	return 0;
}
