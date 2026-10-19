/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <spawn.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void launch(char *const arguments[], char *const environment[])
{
	pid_t child;
	int status;
	assert(posix_spawn(&child, arguments[0], NULL, NULL, arguments,
			   environment) == 0);
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

int main(int argc, char **argv)
{
	if (argc == 1 && !argv[0][0]) {
		assert(getenv("EMPTY_ARGV"));
		return 0;
	}
	if (argc > 1 && !strcmp(argv[1], "many")) {
		assert(argc == 4096);
		for (int i = 2; i < argc; ++i)
			assert(!argv[i][0]);
		int count = 0;
		while (environ[count])
			assert(!strcmp(environ[count++], "X="));
		assert(count == 4096);
		return 0;
	}
	if (argc == 3 && !strcmp(argv[1], "string")) {
		assert(strlen(argv[2]) == 131071);
		return 0;
	}
	char **arguments = calloc(20001, sizeof(*arguments));
	char **environment = calloc(4097, sizeof(*environment));
	char *string = malloc(131073);
	assert(arguments && environment && string);
	for (int i = 0; i < 20000; ++i)
		arguments[i] = "";
	arguments[0] = argv[0];
	arguments[1] = "many";
	arguments[4096] = NULL;
	for (int i = 0; i < 4096; ++i)
		environment[i] = "X=";
	launch(arguments, environment);
	memset(string, 'x', 131072);
	string[131071] = 0;
	char *one[] = { argv[0], "string", string, NULL };
	char *none[] = { NULL };
	launch(one, none);
	string[131071] = 'x';
	string[131072] = 0;
	errno = 0;
	assert(execve(argv[0], one, none) == -1 && errno == E2BIG);
	struct rlimit original, limit;
	assert(getrlimit(RLIMIT_STACK, &original) == 0);
	limit = original;
	limit.rlim_cur = 512 * 1024;
	assert(setrlimit(RLIMIT_STACK, &limit) == 0);
	arguments[4096] = "";
	errno = 0;
	assert(execve(argv[0], arguments, none) == -1 && errno == E2BIG);
	limit.rlim_cur = 8192;
	assert(setrlimit(RLIMIT_STACK, &limit) == 0);
	string[32768] = 0;
	errno = 0;
	assert(execve(argv[0], one, none) == -1 && errno == E2BIG);
	assert(setrlimit(RLIMIT_STACK, &original) == 0);
	pid_t child;
	int status;
	char *empty_environment[] = { "EMPTY_ARGV=1", NULL };
	assert(posix_spawn(&child, argv[0], NULL, NULL, none,
			   empty_environment) == 0);
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	free(string);
	free(environment);
	free(arguments);
	return 0;
}
