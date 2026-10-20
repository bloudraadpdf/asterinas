// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <cpuid.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define SHSTK_STATUS 0x5005
#define SEGV_CONTROL_PROTECTION 10

__attribute__((naked, noinline)) static void mismatch_return_stack(void)
{
	__asm__("mov $0x5001, %edi\n\t"
		"mov $1, %esi\n\t"
		"mov $158, %eax\n\t"
		"syscall\n\t"
		"test %rax, %rax\n\t"
		"js 2f\n\t"
		"sub $8, %rsp\n\t"
		"call 1f\n\t"
		"nop\n\t"
		"1: pop %rax\n\t"
		"add $8, %rsp\n\t"
		"2: ret\n\t");
}

static void control_protection_fault(int signal, siginfo_t *info, void *context)
{
	(void)context;
	if (signal != SIGSEGV)
		_exit(2);
	if (info->si_code != SEGV_CONTROL_PROTECTION)
		_exit(3);
	_exit(info->si_addr == NULL ? 0 : 4);
}

int main(void)
{
	unsigned int eax, ebx, ecx, edx;
	if (!__get_cpuid_count(7, 0, &eax, &ebx, &ecx, &edx) ||
	    !(ecx & (1U << 7)))
		return 77;
	void *probe = (void *)syscall(453, 0, 4096, 0);
	if (probe == MAP_FAILED && (errno == ENOSYS || errno == EOPNOTSUPP))
		return 77;
	assert(probe != MAP_FAILED);
	assert(munmap(probe, 4096) == 0);
	unsigned long features;
	if (syscall(SYS_arch_prctl, SHSTK_STATUS, &features) == -1) {
		assert(errno == EINVAL || errno == EOPNOTSUPP);
		return 77;
	}
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		struct sigaction action = {
			.sa_sigaction = control_protection_fault,
			.sa_flags = SA_SIGINFO,
		};
		assert(sigemptyset(&action.sa_mask) == 0);
		assert(sigaction(SIGSEGV, &action, NULL) == 0);
		mismatch_return_stack();
		_exit(1);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
		fprintf(stderr, "#CP child wait status=%#x\n", status);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	puts("PASS shadow-stack control-protection signal metadata");
	return 0;
}
