/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <sched.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

struct state {
	int tid;
	int fail;
	char *program;
};

static int execute(void *argument)
{
	struct state *state = argument;
	assert(syscall(SYS_set_tid_address, &state->tid) ==
	       syscall(SYS_gettid));
	char *program = state->fail ? "/missing-exec-clear-tid" :
				      state->program;
	char *arguments[] = { program, "after", NULL };
	execve(program, arguments, NULL);
	_exit(state->fail && errno == ENOENT ? 37 : 1);
}

int main(int argc, char **argv)
{
	if (argc == 2 && !strcmp(argv[1], "after"))
		return 0;
	for (int fail = 0; fail < 2; ++fail) {
		struct state state = { .tid = 12345,
				       .fail = fail,
				       .program = argv[0] };
		char *stack = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
				   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		assert(stack != MAP_FAILED);
		pid_t child = clone(execute, stack + 65536, CLONE_VM | SIGCHLD,
				    &state);
		assert(child > 0);
		int status;
		assert(waitpid(child, &status, 0) == child);
		assert(WIFEXITED(status) &&
		       WEXITSTATUS(status) == (fail ? 37 : 0));
		assert(state.tid == 0);
		assert(munmap(stack, 65536) == 0);
	}
	return 0;
}
