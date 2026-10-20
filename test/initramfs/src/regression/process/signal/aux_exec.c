// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/auxv.h>
#include <sys/wait.h>
#include <unistd.h>

static void check(unsigned long key, unsigned long expected)
{
	errno = 0;
	assert(getauxval(key) == expected && errno == 0);
}

int main(int argc, char **argv)
{
	check(AT_UID, getuid());
	check(AT_EUID, geteuid());
	check(AT_GID, getgid());
	check(AT_EGID, getegid());
	if (argc == 2) {
		check(AT_SECURE, strtoul(argv[1], NULL, 10));
		return 0;
	}
	check(AT_SECURE, 0);
	assert(geteuid() == 0);
	int executable = open("/proc/self/exe", O_PATH | O_CLOEXEC);
	assert(executable >= 0);
	for (int mixed = 0; mixed < 2; mixed++) {
		pid_t child = fork();
		assert(child >= 0);
		if (!child) {
			assert(setresgid(60002, 60002, 60002) == 0);
			assert(setresuid(60002, mixed ? 60003 : 60002,
					 mixed ? 60003 : 60002) == 0);
			char *arguments[] = { argv[0], mixed ? "1" : "0",
					      NULL };
			fexecve(executable, arguments, environ);
			_exit(1);
		}
		int status;
		assert(waitpid(child, &status, 0) == child &&
		       WIFEXITED(status) && WEXITSTATUS(status) == 0);
	}
	assert(close(executable) == 0);
	puts("PASS exec auxv identity and secure status for root, user and mixed IDs");
	return 0;
}
