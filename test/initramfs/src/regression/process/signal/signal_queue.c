// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t code, sender, value;

static void receive(int number, siginfo_t *info, void *context)
{
	(void)context;
	assert(number == SIGUSR1);
	code = info->si_code;
	sender = info->si_pid;
	value = info->si_value.sival_int;
}

static void pending_budget(int abandon)
{
	assert(setuid(60001) == 0);
	sigset_t blocked;
	assert(sigemptyset(&blocked) == 0 &&
	       sigaddset(&blocked, SIGRTMIN) == 0);
	assert(sigprocmask(SIG_BLOCK, &blocked, NULL) == 0);
	struct rlimit limit;
	assert(getrlimit(RLIMIT_SIGPENDING, &limit) == 0 &&
	       limit.rlim_max >= 2);
	limit.rlim_cur = 2;
	assert(setrlimit(RLIMIT_SIGPENDING, &limit) == 0);
	assert(sigqueue(getpid(), SIGRTMIN,
			(union sigval){ .sival_int = 11 }) == 0);
	assert(sigqueue(getpid(), SIGRTMIN,
			(union sigval){ .sival_int = 22 }) == 0);
	if (abandon)
		_exit(0);
	assert(sigqueue(getpid(), SIGRTMIN,
			(union sigval){ .sival_int = 33 }) == -1 &&
	       errno == EAGAIN);
	struct timespec zero = { 0, 0 };
	siginfo_t info;
	assert(sigtimedwait(&blocked, &info, &zero) == SIGRTMIN &&
	       info.si_value.sival_int == 11);
	assert(sigqueue(getpid(), SIGRTMIN,
			(union sigval){ .sival_int = 33 }) == 0);
	assert(sigtimedwait(&blocked, &info, &zero) == SIGRTMIN &&
	       info.si_value.sival_int == 22);
	assert(sigtimedwait(&blocked, &info, &zero) == SIGRTMIN &&
	       info.si_value.sival_int == 33);
	limit.rlim_cur = 0;
	assert(setrlimit(RLIMIT_SIGPENDING, &limit) == 0);
	assert(sigqueue(getpid(), SIGUSR1, (union sigval){ .sival_int = 37 }) ==
	       0);
	assert(code == SI_USER && sender == 0 && value == 0);
}

int main(void)
{
	struct sigaction action = { .sa_sigaction = receive,
				    .sa_flags = SA_SIGINFO };
	assert(sigemptyset(&action.sa_mask) == 0 &&
	       sigaction(SIGUSR1, &action, NULL) == 0);
	assert(sigqueue(getpid(), SIGUSR1, (union sigval){ .sival_int = 23 }) ==
	       0);
	assert(code == SI_QUEUE && sender == getpid() && value == 23);
	siginfo_t info;
	memset(&info, 0, sizeof(info));
	info.si_signo = SIGUSR1;
	info.si_code = SI_QUEUE;
	info.si_pid = getpid();
	info.si_uid = getuid();
	info.si_value.sival_int = 29;
	assert(syscall(SYS_rt_tgsigqueueinfo, getpid(), syscall(SYS_gettid),
		       SIGUSR1, &info) == 0);
	assert(code == SI_QUEUE && sender == getpid() && value == 29);
	assert(syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, (void *)1) ==
		       -1 &&
	       errno == EFAULT);
	size_t page = (size_t)sysconf(_SC_PAGESIZE);
	unsigned char *memory = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
				     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED &&
	       mprotect(memory + page, page, PROT_NONE) == 0);
	info.si_value.sival_int = 43;
	memcpy(memory + page - 48, &info, 48);
	assert(syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1,
		       memory + page - 48) == 0 &&
	       value == 43);
	assert(munmap(memory, 2 * page) == 0);
	info.si_code = -100;
	((unsigned char *)&info)[64] = 1;
	assert(syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, &info) == -1 &&
	       errno == E2BIG);
	((unsigned char *)&info)[64] = 0;
	info.si_code = SI_USER;
	assert(syscall(SYS_rt_sigqueueinfo, -1, -1, &info) == -1 &&
	       errno == EPERM);
	assert(syscall(SYS_rt_tgsigqueueinfo, 0, -1, SIGUSR1, &info) == -1 &&
	       errno == EINVAL);
	int retained = -1;
	for (int abandon = 1; abandon >= 0; abandon--) {
		pid_t child = fork();
		assert(child >= 0);
		if (!child) {
			pending_budget(abandon);
			_exit(0);
		}
		if (abandon) {
			retained = (int)syscall(SYS_pidfd_open, child, 0);
			assert(retained >= 0);
		}
		int status;
		assert(waitpid(child, &status, 0) == child &&
		       WIFEXITED(status) && WEXITSTATUS(status) == 0);
	}
	assert(close(retained) == 0);
	puts("PASS queued signal metadata, thread delivery, FIFO and pending quota release");
	return 0;
}
