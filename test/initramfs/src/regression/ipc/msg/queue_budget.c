// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdint.h>
#include <sys/msg.h>

struct message {
	long type;
	char data[8];
};

int main(void)
{
	int id = msgget(IPC_PRIVATE, 0600);
	assert(id >= 0);
	struct msqid_ds info;
	assert(msgctl(id, IPC_STAT, &info) == 0);
	info.msg_qbytes = 2;
	assert(msgctl(id, IPC_SET, &info) == 0);
	struct message message = { .type = 1 };
	assert(msgsnd(id, &message, 0, IPC_NOWAIT) == 0);
	assert(msgsnd(id, &message, 0, IPC_NOWAIT) == 0);
	assert(msgsnd(id, &message, 0, IPC_NOWAIT) == -1 && errno == EAGAIN);
	assert(msgctl(id, IPC_STAT, &info) == 0 && info.msg_qnum == 2);
	assert(msgrcv(id, &message, 0, 0, IPC_NOWAIT) == 0);
	assert(msgsnd(id, &message, 0, IPC_NOWAIT) == 0);
	assert(msgrcv(id, &message, 0, 0, IPC_NOWAIT) == 0);
	assert(msgrcv(id, &message, 0, 0, IPC_NOWAIT) == 0);
	info.msg_qbytes = 8;
	assert(msgctl(id, IPC_SET, &info) == 0);
	assert(msgsnd(id, &message, 8, IPC_NOWAIT) == 0);
	volatile uintptr_t invalid = 1;
	assert(msgrcv(id, (void *)invalid, 8, 0, IPC_NOWAIT) == -1 &&
	       errno == EFAULT);
	assert(msgctl(id, IPC_STAT, &info) == 0 && info.msg_qnum == 0);
	assert(msgsnd(id, &message, 8, IPC_NOWAIT) == 0);
	assert(msgctl(id, IPC_RMID, NULL) == 0);
	int replacement = msgget(IPC_PRIVATE, 0600);
	assert(replacement >= 0 && replacement != id);
	assert(msgsnd(id, &message, 0, IPC_NOWAIT) == -1 && errno == EINVAL);
	assert(msgctl(replacement, IPC_RMID, NULL) == 0);
	puts("PASS message zero-length limits, copy faults, release and stale IDs");
	return 0;
}
