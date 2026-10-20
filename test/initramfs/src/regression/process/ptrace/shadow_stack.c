// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

#include "../../common/yama_ptrace_scope.h"

#define SHSTK_ENABLE 0x5001
#define SHSTK_DISABLE 0x5002
#define SHSTK_LOCK 0x5003
#define SHSTK_UNLOCK 0x5004
#define SHSTK_STATUS 0x5005
#define SHSTK_NOTE 0x204
#define ARCH_PRCTL_REQUEST 30

static int status_has(pid_t child, const char *field, const char *feature)
{
	char path[64], line[256];
	assert(snprintf(path, sizeof(path), "/proc/%d/status", child) > 0);
	FILE *file = fopen(path, "r");
	assert(file != NULL);
	while (fgets(line, sizeof(line), file)) {
		if (strncmp(line, field, strlen(field)) == 0) {
			int found = strstr(line + strlen(field), feature) !=
				    NULL;
			assert(fclose(file) == 0);
			return found;
		}
	}
	assert(0);
	return 0;
}

FN_TEST(shadow_stack)
{
	SKIP_TEST_IF(read_yama_scope() == YAMA_SCOPE_NO_ATTACH);
	int ready[2];
	assert(pipe(ready) == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		assert(close(ready[0]) == 0);
		long result;
		// Enabling cannot return through a libc syscall wrapper's pre-existing frame.
		__asm__ volatile("syscall"
				 : "=a"(result)
				 : "0"((long)SYS_arch_prctl),
				   "D"((long)SHSTK_ENABLE), "S"(1UL)
				 : "rcx", "r11", "memory");
		int active = result == 0;
		assert(active || result == -EOPNOTSUPP || result == -ENOSYS);
		assert(syscall(SYS_arch_prctl, SHSTK_LOCK, 1UL) == 0);
		assert(write(ready[1], &active, sizeof(active)) ==
		       sizeof(active));
		assert(close(ready[1]) == 0);
		assert(ptrace(PTRACE_TRACEME, 0, NULL, NULL) == 0);
		assert(raise(SIGSTOP) == 0);
		if (active)
			assert(syscall(SYS_arch_prctl, SHSTK_DISABLE, 1UL) ==
			       0);
		errno = 0;
		assert(syscall(SYS_arch_prctl, SHSTK_DISABLE, 2UL) == -1 &&
		       errno == EPERM);
		_exit(0);
	}
	assert(close(ready[1]) == 0);
	int active;
	assert(read(ready[0], &active, sizeof(active)) == sizeof(active));
	assert(close(ready[0]) == 0);
	int status;
	assert(waitpid(child, &status, 0) == child && WIFSTOPPED(status));
	uint64_t features = UINT64_MAX;
	assert(ptrace(ARCH_PRCTL_REQUEST, child, &features, SHSTK_STATUS) == 0);
	assert(features == (uint64_t)active);
	assert(status_has(child, "x86_Thread_features:", "shstk") == active);
	assert(status_has(child, "x86_Thread_features_locked:", "shstk"));

	uint64_t pointer = 0;
	struct iovec vector = { &pointer, sizeof(pointer) };
	errno = 0;
	long result = ptrace(PTRACE_GETREGSET, child, SHSTK_NOTE, &vector);
	if (active) {
		assert(result == 0 && vector.iov_len == sizeof(pointer));
		assert(pointer != 0 && pointer % 8 == 0);
		uint64_t original = pointer;
		struct user_regs_struct registers;
		assert(ptrace(PTRACE_GETREGS, child, NULL, &registers) == 0);
		assert(ptrace(PTRACE_SETREGS, child, NULL, &registers) == 0);
		assert(ptrace(PTRACE_GETREGSET, child, SHSTK_NOTE, &vector) ==
		       0);
		assert(pointer == original);
		pointer++;
		errno = 0;
		assert(ptrace(PTRACE_SETREGSET, child, SHSTK_NOTE, &vector) ==
			       -1 &&
		       errno == EINVAL);
		pointer = 0;
		assert(ptrace(PTRACE_SETREGSET, child, SHSTK_NOTE, &vector) ==
		       0);
		pointer = UINT64_MAX;
		assert(ptrace(PTRACE_GETREGSET, child, SHSTK_NOTE, &vector) ==
			       0 &&
		       pointer == 0);
		pointer = original;
		assert(ptrace(PTRACE_SETREGSET, child, SHSTK_NOTE, &vector) ==
		       0);
	} else {
		assert(result == -1 && errno == ENODEV);
	}
	vector.iov_len = 7;
	errno = 0;
	assert(ptrace(PTRACE_GETREGSET, child, SHSTK_NOTE, &vector) == -1 &&
	       errno == EINVAL);
	errno = 0;
	assert(ptrace(ARCH_PRCTL_REQUEST, child, 1UL, SHSTK_ENABLE) == -1 &&
	       errno == EINVAL);
	assert(ptrace(ARCH_PRCTL_REQUEST, child, 1UL, SHSTK_UNLOCK) == 0);
	assert(!status_has(child, "x86_Thread_features_locked:", "shstk"));
	assert(ptrace(ARCH_PRCTL_REQUEST, child, 2UL, SHSTK_LOCK) == 0);
	assert(status_has(child, "x86_Thread_features_locked:", "wrss"));
	assert(ptrace(PTRACE_CONT, child, NULL, NULL) == 0);
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
END_TEST()
