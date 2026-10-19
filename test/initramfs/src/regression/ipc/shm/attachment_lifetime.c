// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/shm.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	struct shmid_ds info;
	if (argc == 2) {
		assert(shmctl(atoi(argv[1]), IPC_STAT, &info) == 0);
		assert(info.shm_nattch == 2);
		return 0;
	}
	int id = shmget(IPC_PRIVATE, 4096, 0600);
	assert(id >= 0);
	char *first = shmat(id, NULL, 0);
	char *second = shmat(id, NULL, 0);
	assert(first != (void *)-1 && second != (void *)-1 && first != second);
	first[0] = 37;
	assert(second[0] == 37);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		assert(shmctl(id, IPC_STAT, &info) == 0);
		assert(info.shm_nattch == 4 && first[0] == 37 &&
		       second[0] == 37);
		char number[24];
		snprintf(number, sizeof(number), "%d", id);
		execl(argv[0], argv[0], number, (char *)NULL);
		_exit(1);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	assert(shmctl(id, IPC_STAT, &info) == 0 && info.shm_nattch == 2);
	assert(shmctl(id, IPC_RMID, NULL) == 0);
	assert(shmdt(first) == 0 && shmdt(second) == 0);
	key_t key = 0x65000000 ^ getpid();
	id = shmget(key, 3 * 4096, IPC_CREAT | IPC_EXCL | 0600);
	assert(id >= 0);
	first = shmat(id, NULL, 0);
	assert(first != (void *)-1);
	assert(mprotect(first + 4096, 4096, PROT_READ) == 0);
	assert(shmctl(id, IPC_STAT, &info) == 0 && info.shm_nattch == 3);
	assert(munmap(first + 4096, 4096) == 0);
	char *hole = mmap(first + 4096, 4096, PROT_READ | PROT_WRITE,
			  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
	assert(hole == first + 4096);
	hole[0] = 59;
	assert(shmctl(id, IPC_RMID, NULL) == 0);
	assert(shmget(key, 0, 0) == -1 && errno == ENOENT);
	int replacement = shmget(key, 4096, IPC_CREAT | IPC_EXCL | 0600);
	assert(replacement >= 0 && replacement != id);
	assert(shmdt(first) == 0 && hole[0] == 59);
	assert(shmctl(id, IPC_STAT, &info) == -1 && errno == EINVAL);
	assert(shmget(key, 0, 0) == replacement);
	assert(munmap(hole, 4096) == 0);
	second = shmat(replacement, NULL, SHM_RDONLY);
	assert(second != (void *)-1);
	assert(mprotect(second, 4096, PROT_READ | PROT_WRITE) == -1 &&
	       errno == EACCES);
	assert(shmdt(second) == 0);
	assert(shmctl(replacement, IPC_RMID, NULL) == 0);
	puts("PASS shared-memory fork, exec and final attachment release");
	return 0;
}
