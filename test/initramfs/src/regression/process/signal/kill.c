// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <pthread.h>
#include <signal.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>

#include "../../common/test.h"

FN_SETUP(setpgrp)
{
	CHECK(setpgrp());
}
END_SETUP()

FN_TEST(kill_dead_process)
{
	pid_t ppid, cpid;
	int status;

	ppid = TEST_SUCC(getpid());
	cpid = TEST_SUCC(fork());
	if (cpid == 0) {
		exit(EXIT_SUCCESS);
	}
	usleep(200 * 1000);

	TEST_SUCC(kill(ppid, SIGCHLD));
	TEST_SUCC(kill(-ppid, SIGCHLD));

	// Killing dead processes will succeed.
	TEST_SUCC(kill(cpid, SIGCHLD));
	TEST_ERRNO(kill(-cpid, SIGCHLD), ESRCH);

	TEST_RES(wait(&status),
		 _ret == cpid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
END_TEST()

struct nonleader_target {
	pthread_barrier_t ready;
	pthread_barrier_t release;
	pid_t tid;
	uid_t uid;
};

static void *blocked_nonleader(void *data)
{
	struct nonleader_target *target = data;

	/* Keep the leader's credentials unchanged. */
	if (target->uid != getuid())
		CHECK(syscall(SYS_setresuid, target->uid, target->uid,
			      target->uid));
	target->tid = CHECK(syscall(SYS_gettid));
	CHECK_WITH(pthread_barrier_wait(&target->ready),
		   _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);
	CHECK_WITH(pthread_barrier_wait(&target->release),
		   _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);
	return NULL;
}

FN_TEST(kill_nonleader_sends_process_signal)
{
	sigset_t blocked, saved;
	CHECK(sigemptyset(&blocked));
	CHECK(sigaddset(&blocked, SIGUSR1));
	CHECK(sigprocmask(SIG_BLOCK, &blocked, &saved));
	struct nonleader_target target = {
		.uid = getuid() == 0 ? 60001 : getuid(),
	};
	CHECK_WITH(pthread_barrier_init(&target.ready, NULL, 2), _ret == 0);
	CHECK_WITH(pthread_barrier_init(&target.release, NULL, 2), _ret == 0);
	pthread_t thread;
	CHECK_WITH(pthread_create(&thread, NULL, blocked_nonleader, &target),
		   _ret == 0);
	CHECK_WITH(pthread_barrier_wait(&target.ready),
		   _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);

	TEST_RES(target.tid, _ret != getpid());
	TEST_SUCC(kill(target.tid, 0));
	if (getuid() == 0) {
		pid_t child = CHECK(fork());
		if (child == 0) {
			CHECK(setresuid(target.uid, target.uid, target.uid));
			CHECK_WITH(kill(target.tid, 0), _ret == 0);
			CHECK_WITH(kill(getppid(), 0),
				   _ret == -1 && errno == EPERM);
			_exit(EXIT_SUCCESS);
		}
		int status;
		TEST_RES(waitpid(child, &status, 0),
			 _ret == child && WIFEXITED(status) &&
				 WEXITSTATUS(status) == EXIT_SUCCESS);
	}
	TEST_SUCC(kill(target.tid, SIGUSR1));
	struct timespec timeout = { .tv_sec = 5 };
	siginfo_t info;
	TEST_RES(sigtimedwait(&blocked, &info, &timeout),
		 _ret == SIGUSR1 && info.si_code == SI_USER &&
			 info.si_pid == getpid() && info.si_uid == getuid());

	CHECK_WITH(pthread_barrier_wait(&target.release),
		   _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);
	TEST_RES(pthread_join(thread, NULL), _ret == 0);
	TEST_RES(pthread_barrier_destroy(&target.ready), _ret == 0);
	TEST_RES(pthread_barrier_destroy(&target.release), _ret == 0);
	TEST_SUCC(sigprocmask(SIG_SETMASK, &saved, NULL));
}
END_TEST()

FN_TEST(kill_dead_group)
{
	pid_t ppid, cpid;
	int status;

	ppid = TEST_SUCC(getpid());
	cpid = TEST_SUCC(fork());
	if (cpid == 0) {
		CHECK(setpgrp());
		exit(EXIT_SUCCESS);
	}
	usleep(200 * 1000);

	TEST_SUCC(kill(ppid, SIGCHLD));
	TEST_SUCC(kill(-ppid, SIGCHLD));

	// Killing dead process groups will succeed.
	TEST_SUCC(kill(cpid, SIGCHLD));
	TEST_SUCC(kill(-cpid, SIGCHLD));

	TEST_RES(wait(&status),
		 _ret == cpid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
END_TEST()

static void *background_thread(void *data)
{
	sleep(3600);
	return NULL;
}

FN_TEST(kill_dead_thread)
{
	pid_t cpid;
	int status;

	cpid = TEST_SUCC(fork());
	if (cpid == 0) {
		pthread_t tid;
		CHECK_WITH(pthread_create(&tid, NULL, background_thread, NULL),
			   _ret == 0);
		syscall(SYS_exit, 0);
	}
	usleep(200 * 1000);

	// Killing dead threads will succeed.
	TEST_SUCC(kill(cpid, SIGCHLD));
	TEST_SUCC(tgkill(cpid, cpid, SIGCHLD));
	TEST_SUCC(syscall(SYS_tkill, cpid, SIGCHLD));

	TEST_SUCC(kill(cpid, SIGKILL));
	TEST_RES(wait(&status), _ret == cpid && WIFSIGNALED(status) &&
					WTERMSIG(status) == SIGKILL);
}
END_TEST()
