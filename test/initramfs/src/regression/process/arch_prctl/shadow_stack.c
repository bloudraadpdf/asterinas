// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <cpuid.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define SHSTK_ENABLE 0x5001
#define SHSTK_DISABLE 0x5002
#define SHSTK_LOCK 0x5003
#define SHSTK_UNLOCK 0x5004
#define SHSTK_STATUS 0x5005
#define FEATURE_SHSTK 1UL
#define FEATURE_WRSS 2UL
#define MAP_SHADOW_STACK_NR 453
#define CLONE3_NR 435
#define PAGE 4096UL

static unsigned long feature_status(void)
{
	unsigned long features = ~0UL;
	assert(syscall(SYS_arch_prctl, SHSTK_STATUS, &features) == 0);
	return features;
}

static uintptr_t read_ssp(void)
{
	uintptr_t pointer = 0;
	__asm__ volatile("rdsspq %0" : "+r"(pointer));
	return pointer;
}

static void wrss(volatile unsigned long *pointer, unsigned long value)
{
	__asm__ volatile("wrssq %1, (%0)"
			 :
			 : "r"(pointer), "r"(value)
			 : "memory");
}

// The helper enables CET before calling C and disables it before its untracked return.
__attribute__((naked, noinline)) static long
with_shadow_stack(int (*body)(void))
{
	__asm__("push %r12\n\t"
		"push %r13\n\t"
		"sub $8, %rsp\n\t"
		"mov %rdi, %r12\n\t"
		"mov $0x5001, %edi\n\t"
		"mov $1, %esi\n\t"
		"mov $158, %eax\n\t"
		"syscall\n\t"
		"test %rax, %rax\n\t"
		"js 1f\n\t"
		"call *%r12\n\t"
		"mov %eax, %r13d\n\t"
		"mov $0x5002, %edi\n\t"
		"mov $1, %esi\n\t"
		"mov $158, %eax\n\t"
		"syscall\n\t"
		"test %rax, %rax\n\t"
		"js 2f\n\t"
		"mov %r13d, %eax\n\t"
		"1: add $8, %rsp\n\t"
		"pop %r13\n\t"
		"pop %r12\n\t"
		"ret\n\t"
		"2: mov $99, %edi\n\t"
		"mov $231, %eax\n\t"
		"syscall\n\t"
		"ud2\n\t");
}

static void expect_error(long operation, unsigned long argument, int error)
{
	errno = 0;
	long result = syscall(SYS_arch_prctl, operation, argument);
	if (result != -1 || errno != error)
		fprintf(stderr,
			"arch_prctl(%#lx, %#lx): result=%ld errno=%d expected=%d\n",
			operation, argument, result, errno, error);
	assert(result == -1);
	assert(errno == error);
}

static void wait_success(pid_t child)
{
	int status;
	assert(child > 0);
	assert(waitpid(child, &status, 0) == child);
	if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
		fprintf(stderr, "child %ld wait status=%#x\n", (long)child,
			status);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static void run_enabled(const char *name, int (*body)(void))
{
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		assert(with_shadow_stack(body) == 0);
		assert(feature_status() == 0);
		_exit(0);
	}
	wait_success(child);
	printf("PASS %s\n", name);
}

static int controls(void)
{
	assert(feature_status() == FEATURE_SHSTK);
	assert(read_ssp() != 0);
	expect_error(SHSTK_ENABLE, 0, EINVAL);
	expect_error(SHSTK_ENABLE, 3, EINVAL);
	expect_error(SHSTK_ENABLE, 4, EINVAL);
	expect_error(SHSTK_STATUS, 1, EFAULT);
	assert(syscall(SYS_arch_prctl, SHSTK_UNLOCK, FEATURE_SHSTK) == 0);
	assert(syscall(SYS_arch_prctl, SHSTK_ENABLE, FEATURE_WRSS) == 0);
	assert(feature_status() == (FEATURE_SHSTK | FEATURE_WRSS));
	assert(syscall(SYS_arch_prctl, SHSTK_DISABLE, FEATURE_WRSS) == 0);
	assert(feature_status() == FEATURE_SHSTK);
	assert(syscall(SYS_arch_prctl, SHSTK_LOCK, FEATURE_WRSS) == 0);
	expect_error(SHSTK_ENABLE, FEATURE_WRSS, EPERM);
	assert(syscall(SYS_arch_prctl, SHSTK_LOCK, FEATURE_SHSTK) == 0);
	expect_error(SHSTK_DISABLE, FEATURE_SHSTK, EPERM);
	expect_error(SHSTK_ENABLE, FEATURE_SHSTK, EPERM);
	expect_error(SHSTK_UNLOCK, FEATURE_SHSTK, EPERM);
	_exit(0);
}

static volatile unsigned long *shadow_word;
static volatile sig_atomic_t expected_fault_signal;

static void memory_fault(int signal, siginfo_t *info, void *context)
{
	(void)context;
	if (signal != expected_fault_signal)
		_exit(2);
	if (signal == SIGSEGV &&
	    (info->si_code != SEGV_ACCERR || info->si_addr != shadow_word))
		_exit(3);
	_exit(0);
}

static int normal_write_fault(void)
{
	*shadow_word = 1;
	return 1;
}

static int disabled_wrss_fault(void)
{
	wrss(shadow_word, 1);
	return 1;
}

static int readonly_wrss_fault(void)
{
	assert(syscall(SYS_arch_prctl, SHSTK_ENABLE, FEATURE_WRSS) == 0);
	wrss(shadow_word, 1);
	return 1;
}

static void run_fault(int signal, int (*body)(void))
{
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		expected_fault_signal = signal;
		struct sigaction action = {
			.sa_sigaction = memory_fault,
			.sa_flags = SA_SIGINFO,
		};
		assert(sigemptyset(&action.sa_mask) == 0);
		assert(sigaction(signal, &action, NULL) == 0);
		with_shadow_stack(body);
		_exit(1);
	}
	wait_success(child);
}

static void mapping_contract(void)
{
	shadow_word = (void *)syscall(MAP_SHADOW_STACK_NR, 0, PAGE, 1);
	assert(shadow_word != MAP_FAILED);
	uintptr_t top = (uintptr_t)shadow_word + PAGE;
	assert(shadow_word[PAGE / sizeof(unsigned long) - 1] == (top | 1));
	assert(shadow_word[0] == 0);
	run_fault(SIGSEGV, normal_write_fault);
	run_fault(SIGILL, disabled_wrss_fault);
	assert(mprotect((void *)shadow_word, PAGE, PROT_READ) == 0);
	run_fault(SIGSEGV, readonly_wrss_fault);
	assert(munmap((void *)shadow_word, PAGE) == 0);

	errno = 0;
	assert(syscall(MAP_SHADOW_STACK_NR, 0, PAGE, 2) == -1 &&
	       errno == EINVAL);
	errno = 0;
	assert(syscall(MAP_SHADOW_STACK_NR, PAGE, PAGE, 0) == -1 &&
	       errno == ERANGE);
	errno = 0;
	assert(syscall(MAP_SHADOW_STACK_NR, 0, 7, 1) == -1 && errno == ENOSPC);
	errno = 0;
	assert(syscall(MAP_SHADOW_STACK_NR, 0, 0, 0) == -1 && errno == EINVAL);
	puts("PASS shadow mapping token and write permissions");
}

static uintptr_t signal_ssp[3];
static volatile sig_atomic_t signal_depth;
static uintptr_t altstack_start;
static uintptr_t altstack_end;

static void nested_signal(int signal, siginfo_t *info, void *context)
{
	(void)info;
	(void)context;
	char stack_byte;
	if (signal != SIGUSR1 || (uintptr_t)&stack_byte < altstack_start ||
	    (uintptr_t)&stack_byte >= altstack_end)
		_exit(4);
	int depth = signal_depth++;
	if (depth >= 3 || feature_status() != FEATURE_SHSTK)
		_exit(5);
	signal_ssp[depth] = read_ssp();
	if (!signal_ssp[depth] ||
	    (depth && signal_ssp[depth] >= signal_ssp[depth - 1]))
		_exit(6);
	if (depth < 2)
		assert(raise(SIGUSR1) == 0);
	assert(sched_yield() == 0);
	if (read_ssp() != signal_ssp[depth])
		_exit(7);
	signal_depth--;
}

static int signals(void)
{
	void *stack = mmap(NULL, 128 * 1024, PROT_READ | PROT_WRITE,
			   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(stack != MAP_FAILED);
	altstack_start = (uintptr_t)stack;
	altstack_end = altstack_start + 128 * 1024;
	stack_t alternate = { .ss_sp = stack, .ss_size = 128 * 1024 };
	assert(sigaltstack(&alternate, NULL) == 0);
	struct sigaction action = {
		.sa_sigaction = nested_signal,
		.sa_flags = SA_SIGINFO | SA_NODEFER | SA_ONSTACK,
	};
	assert(sigemptyset(&action.sa_mask) == 0);
	assert(sigaction(SIGUSR1, &action, NULL) == 0);
	uintptr_t before = read_ssp();
	assert(raise(SIGUSR1) == 0);
	assert(read_ssp() == before && signal_depth == 0);
	assert(signal_ssp[0] && signal_ssp[1] && signal_ssp[2]);
	alternate.ss_flags = SS_DISABLE;
	assert(sigaltstack(&alternate, NULL) == 0);
	assert(munmap(stack, 128 * 1024) == 0);
	return 0;
}

static char executable[] = "/proc/self/exe";
static char *const exec_arguments[] = { executable, "--exec-check", NULL };

__attribute__((naked, noinline)) static long vfork_exec(void)
{
	__asm__("mov $58, %eax\n\t"
		"syscall\n\t"
		"test %rax, %rax\n\t"
		"jnz 1f\n\t"
		"lea executable(%rip), %rdi\n\t"
		"lea exec_arguments(%rip), %rsi\n\t"
		"xor %edx, %edx\n\t"
		"mov $59, %eax\n\t"
		"syscall\n\t"
		"mov $127, %edi\n\t"
		"mov $60, %eax\n\t"
		"syscall\n\t"
		"ud2\n\t"
		"1: ret\n\t");
}

static int fork_exec_and_cow(void)
{
	shadow_word = (void *)syscall(MAP_SHADOW_STACK_NR, 0, PAGE, 0);
	assert(shadow_word != MAP_FAILED);
	assert(syscall(SYS_arch_prctl, SHSTK_ENABLE, FEATURE_WRSS) == 0);
	wrss(shadow_word, 0x1111);
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		assert(feature_status() == (FEATURE_SHSTK | FEATURE_WRSS));
		wrss(shadow_word, 0x2222);
		assert(*shadow_word == 0x2222);
		_exit(0);
	}
	wait_success(child);
	assert(*shadow_word == 0x1111);
	assert(syscall(SYS_arch_prctl, SHSTK_DISABLE, FEATURE_WRSS) == 0);
	assert(munmap((void *)shadow_word, PAGE) == 0);
	child = fork();
	assert(child >= 0);
	if (child == 0) {
		execv(executable, exec_arguments);
		_exit(127);
	}
	wait_success(child);
	wait_success(vfork_exec());
	assert(feature_status() == FEATURE_SHSTK);
	return 0;
}

static uintptr_t parent_ssp;
static volatile int clone_seen;

__attribute__((used, noinline)) static int clone_body(void)
{
	if (feature_status() != FEATURE_SHSTK || !read_ssp() ||
	    read_ssp() == parent_ssp)
		return 1;
	clone_seen++;
	return 0;
}

__attribute__((naked, noinline)) static long
spawn_clone(long number, unsigned long first, unsigned long second)
{
	__asm__("mov %rdi, %rax\n\t"
		"mov %rsi, %rdi\n\t"
		"mov %rdx, %rsi\n\t"
		"xor %edx, %edx\n\t"
		"xor %r10d, %r10d\n\t"
		"xor %r8d, %r8d\n\t"
		"syscall\n\t"
		"test %rax, %rax\n\t"
		"jnz 1f\n\t"
		"xor %ebp, %ebp\n\t"
		"call clone_body\n\t"
		"mov %eax, %edi\n\t"
		"mov $60, %eax\n\t"
		"syscall\n\t"
		"ud2\n\t"
		"1: ret\n\t");
}

static size_t mapping_count(void)
{
	int fd = open("/proc/self/maps", O_RDONLY);
	assert(fd >= 0);
	char buffer[4096];
	size_t count = 0;
	ssize_t length;
	while ((length = read(fd, buffer, sizeof(buffer))) > 0)
		for (ssize_t i = 0; i < length; i++)
			count += buffer[i] == '\n';
	assert(length == 0);
	assert(close(fd) == 0);
	return count;
}

static int clones(void)
{
	const size_t size = 128 * 1024;
	void *stack = mmap(NULL, size, PROT_READ | PROT_WRITE,
			   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(stack != MAP_FAILED);
	parent_ssp = read_ssp();
	size_t mappings = mapping_count();
	for (int i = 0; i < 16; i++)
		wait_success(spawn_clone(SYS_clone, CLONE_VM | SIGCHLD,
					 (uintptr_t)stack + size));
	struct clone_args arguments = {
		.flags = CLONE_VM,
		.exit_signal = SIGCHLD,
		.stack = (uintptr_t)stack,
		.stack_size = size,
	};
	wait_success(spawn_clone(CLONE3_NR, (uintptr_t)&arguments,
				 sizeof(arguments)));
	assert(clone_seen == 17);
	assert(mapping_count() == mappings);
	assert(feature_status() == FEATURE_SHSTK);
	assert(munmap(stack, size) == 0);
	return 0;
}

static int migration(void)
{
	cpu_set_t allowed;
	assert(sched_getaffinity(0, sizeof(allowed), &allowed) == 0);
	uintptr_t before = read_ssp();
	int visited = 0;
	for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) {
		if (!CPU_ISSET(cpu, &allowed))
			continue;
		cpu_set_t one;
		CPU_ZERO(&one);
		CPU_SET(cpu, &one);
		assert(sched_setaffinity(0, sizeof(one), &one) == 0);
		unsigned int actual;
		assert(syscall(SYS_getcpu, &actual, NULL, NULL) == 0);
		assert(actual == (unsigned int)cpu);
		assert(sched_getcpu() == cpu);
		assert(sched_yield() == 0);
		assert(feature_status() == FEATURE_SHSTK &&
		       read_ssp() == before);
		visited++;
	}
	assert(visited > 0);
	if (visited == 1)
		puts("SKIP cross-CPU transfer: one allowed CPU");
	assert(sched_setaffinity(0, sizeof(allowed), &allowed) == 0);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc == 2 && strcmp(argv[1], "--exec-check") == 0) {
		assert(feature_status() == 0);
		return 0;
	}
	unsigned int eax, ebx, ecx, edx;
	if (!__get_cpuid_count(7, 0, &eax, &ebx, &ecx, &edx) ||
	    !(ecx & (1U << 7)))
		return 77;
	void *probe = (void *)syscall(MAP_SHADOW_STACK_NR, 0, PAGE, 0);
	if (probe == MAP_FAILED && (errno == ENOSYS || errno == EOPNOTSUPP))
		return 77;
	assert(probe != MAP_FAILED);
	assert(munmap(probe, PAGE) == 0);
	assert(feature_status() == 0);
	expect_error(SHSTK_ENABLE, FEATURE_WRSS, EPERM);
	run_enabled("shadow stack control and locking", controls);
	mapping_contract();
	run_enabled("nested signals on alternate stack", signals);
	run_enabled("fork COW, vfork and exec reset", fork_exec_and_cow);
	run_enabled("clone and clone3 ownership", clones);
	run_enabled("shadow stack CPU migration", migration);
	return 0;
}
