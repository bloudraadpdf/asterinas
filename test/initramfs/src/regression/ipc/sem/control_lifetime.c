// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <fcntl.h>
#include <sched.h>
#include <sys/sem.h>
#include <sys/wait.h>
#include <unistd.h>

union argument {
	int value;
	struct semid_ds *status;
	unsigned short *values;
};

int main(int argc, char **argv)
{
	if (argc == 2)
		return 0;
	key_t key = 0x63000000 ^ getpid();
	int first = semget(key, 2, IPC_CREAT | IPC_EXCL | 0600);
	int second = semget(key + 32000, 2, IPC_CREAT | IPC_EXCL | 0600);
	assert(first >= 0 && second >= 0 && first != second);
	assert(semget(key, 0, 0) == first);
	assert(semget(key + 32000, 0, 0) == second);
	unsigned short values[2] = { 2, 3 }, output[2] = { 0, 0 };
	assert(semctl(first, 0, SETALL, (union argument){ .values = values }) ==
	       0);
	assert(semctl(first, 0, GETALL, (union argument){ .values = output }) ==
	       0);
	assert(output[0] == 2 && output[1] == 3);
	values[0] = 4;
	values[1] = 65535;
	assert(semctl(first, 0, SETALL, (union argument){ .values = values }) ==
		       -1 &&
	       errno == ERANGE);
	assert(semctl(first, 0, SETALL,
		      (union argument){ .values = (void *)1 }) == -1 &&
	       errno == EFAULT);
	assert(semctl(first, 0, GETALL, (union argument){ .values = output }) ==
	       0);
	assert(output[0] == 2 && output[1] == 3);
	struct sembuf transaction[] = { { 0, 1, 0 }, { 0, -3, 0 } };
	assert(semop(first, transaction, 2) == 0);
	assert(semctl(first, 0, GETVAL) == 0);
	transaction[1].sem_op = -2;
	transaction[1].sem_flg = IPC_NOWAIT;
	assert(semop(first, transaction, 2) == -1 && errno == EAGAIN);
	assert(semctl(first, 0, GETVAL) == 0);
	struct semid_ds info;
	assert(semctl(first, 0, IPC_STAT,
		      (union argument){ .status = &info }) == 0);
	info.sem_perm.mode = 0600;
	assert(semctl(first, 0, IPC_SET, (union argument){ .status = &info }) ==
	       0);
	assert(geteuid() == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		assert(setgid(65534) == 0 && setuid(65534) == 0);
		assert(semctl(first, 0, GETVAL) == -1 && errno == EACCES);
		assert(semctl(first, 0, IPC_RMID) == -1 && errno == EPERM);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	child = fork();
	assert(child >= 0);
	if (!child) {
		struct sembuf undo = { 0, 1, SEM_UNDO };
		unsigned short reset[2] = { 7, 8 };
		assert(semop(first, &undo, 1) == 0);
		assert(semctl(first, 0, SETALL,
			      (union argument){ .values = reset }) == 0);
		execl(argv[0], argv[0], "exit", (char *)NULL);
		_exit(1);
	}
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	assert(semctl(first, 0, GETALL, (union argument){ .values = output }) ==
	       0);
	assert(output[0] == 7 && output[1] == 8);
	struct sembuf undo = { 0, 1, SEM_UNDO };
	assert(semop(first, &undo, 1) == 0);
	assert(unshare(CLONE_SYSVSEM) == 0);
	assert(semctl(first, 0, GETVAL) == 7);
	assert(semop(first, &undo, 1) == 0);
	int namespace = open("/proc/self/ns/ipc", O_RDONLY);
	assert(namespace >= 0 && setns(namespace, 0) == 0);
	assert(close(namespace) == 0 && semctl(first, 0, GETVAL) == 7);
	assert(semctl(first, 0, IPC_RMID) == 0);
	int replacement = semget(key, 2, IPC_CREAT | IPC_EXCL | 0600);
	assert(replacement >= 0 && replacement != first);
	assert(semctl(first, 0, GETVAL) == -1 && errno == EINVAL);
	assert(semctl(second, 0, IPC_RMID) == 0);
	assert(semctl(replacement, 0, IPC_RMID) == 0);
	puts("PASS semaphore keys, arrays, transactions, permissions and stale IDs");
	return 0;
}
