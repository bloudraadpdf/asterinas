// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <dirent.h>
#include <fcntl.h>
#include <linux/sched.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include "../common/test.h"

FN_SETUP(pid_namespace_cpu_topology)
{
	cpu_set_t cpus;
	CHECK(sched_getaffinity(0, sizeof(cpus), &cpus));
	fprintf(stderr, "pid_namespace_cpus: %d\n", CPU_COUNT(&cpus));
}
END_SETUP()

static int run_child(void (*body)(void))
{
	pid_t child = CHECK(fork());
	if (child == 0) {
		body();
		_exit(EXIT_SUCCESS);
	}
	int status;
	CHECK_WITH(waitpid(child, &status, 0), _ret == child);
	return WIFEXITED(status) && WEXITSTATUS(status) == EXIT_SUCCESS ? 0 :
									  -1;
}

static void wait_success(pid_t child)
{
	int status;
	CHECK_WITH(waitpid(child, &status, 0),
		   _ret == child && WIFEXITED(status) &&
			   WEXITSTATUS(status) == 0);
}

static void private_proc(void)
{
	CHECK(unshare(CLONE_NEWNS));
	CHECK(mount(NULL, "/", NULL, MS_PRIVATE | MS_REC, NULL));
	CHECK_WITH(mkdir("/proc", 0555), _ret == 0 || errno == EEXIST);
	CHECK(mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC,
		    NULL));
}

static void check_self_link(pid_t pid)
{
	char actual[32];
	char expected[32];
	int length = snprintf(expected, sizeof(expected), "%d", pid);
	CHECK_WITH(readlink("/proc/self", actual, sizeof(actual)),
		   _ret == length && !memcmp(actual, expected, length));
}

static void run_namespace_init(void)
{
	CHECK_WITH(getpid(), _ret == 1);
	CHECK_WITH(getppid(), _ret == 0);
	CHECK_WITH(syscall(SYS_gettid), _ret == 1);
	int first_tid = 0;
	int second_tid = 0;
	CHECK_WITH(syscall(SYS_set_tid_address, &first_tid), _ret == 1);
	CHECK_WITH(syscall(SYS_set_tid_address, &second_tid), _ret == 1);
	CHECK(mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC,
		    NULL));
	check_self_link(1);

	DIR *directory = opendir("/proc");
	CHECK_WITH(directory, _ret != NULL);
	int processes = 0;
	struct dirent *entry;
	while ((entry = readdir(directory))) {
		char *end;
		long pid = strtol(entry->d_name, &end, 10);
		if (end == entry->d_name || *end)
			continue;
		CHECK_WITH(pid, _ret == 1);
		processes++;
	}
	CHECK_WITH(processes, _ret == 1);
	CHECK(closedir(directory));

	pid_t child = CHECK(fork());
	if (child == 0) {
		CHECK_WITH(getpid(), _ret == 2);
		CHECK_WITH(getppid(), _ret == 1);
		CHECK_WITH(syscall(SYS_gettid), _ret == 2);
		check_self_link(2);
		_exit(EXIT_SUCCESS);
	}
	CHECK_WITH(child, _ret == 2);
	int status;
	CHECK_WITH(waitpid(child, &status, 0),
		   _ret == child && WIFEXITED(status) &&
			   WEXITSTATUS(status) == EXIT_SUCCESS);
	_exit(EXIT_SUCCESS);
}

FN_TEST(pid_namespace_preserves_creator_and_scopes_child_identifiers)
{
	pid_t worker = TEST_SUCC(fork());
	if (worker == 0) {
		CHECK(unshare(CLONE_NEWNS));
		CHECK(mount(NULL, "/", NULL, MS_PRIVATE | MS_REC, NULL));
		CHECK_WITH(mkdir("/proc", 0555), _ret == 0 || errno == EEXIST);
		pid_t creator = getpid();
		CHECK(unshare(CLONE_NEWPID));
		CHECK_WITH(getpid(), _ret == creator);
		CHECK_WITH(unshare(CLONE_NEWPID),
			   _ret == -1 && errno == EINVAL);
		pid_t init = CHECK(fork());
		if (init == 0)
			run_namespace_init();
		int status;
		CHECK_WITH(waitpid(init, &status, 0),
			   _ret == init && WIFEXITED(status) &&
				   WEXITSTATUS(status) == EXIT_SUCCESS);
		CHECK_WITH(fork(), _ret == -1 && errno == ENOMEM);
		_exit(EXIT_SUCCESS);
	}
	int status;
	TEST_RES(waitpid(worker, &status, 0),
		 _ret == worker && WIFEXITED(status) &&
			 WEXITSTATUS(status) == EXIT_SUCCESS);
}
END_TEST()

static void check_socket_credentials(void)
{
	int sockets[2];
	CHECK(socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, sockets));
	int pass_credentials = 1;
	CHECK(setsockopt(sockets[0], SOL_SOCKET, SO_PASSCRED, &pass_credentials,
			 sizeof(pass_credentials)));
	CHECK(unshare(CLONE_NEWPID));
	pid_t init = CHECK(fork());
	if (init == 0) {
		struct ucred peer;
		socklen_t length = sizeof(peer);
		CHECK(getsockopt(sockets[1], SOL_SOCKET, SO_PEERCRED, &peer,
				 &length));
		CHECK_WITH(peer.pid, _ret == 0);
		CHECK_WITH(send(sockets[1], "c", 1, 0), _ret == 1);
		_exit(EXIT_SUCCESS);
	}
	char byte;
	union {
		struct cmsghdr alignment;
		char bytes[CMSG_SPACE(sizeof(struct ucred))];
	} control;
	struct iovec data = { .iov_base = &byte, .iov_len = 1 };
	struct msghdr message = {
		.msg_iov = &data,
		.msg_iovlen = 1,
		.msg_control = control.bytes,
		.msg_controllen = sizeof(control),
	};
	CHECK_WITH(recvmsg(sockets[0], &message, 0), _ret == 1);
	struct cmsghdr *header = CMSG_FIRSTHDR(&message);
	CHECK_WITH(header,
		   _ret != NULL && header->cmsg_level == SOL_SOCKET &&
			   header->cmsg_type == SCM_CREDENTIALS &&
			   header->cmsg_len == CMSG_LEN(sizeof(struct ucred)));
	struct ucred sender;
	memcpy(&sender, CMSG_DATA(header), sizeof(sender));
	CHECK_WITH(sender.pid, _ret == init);
	wait_success(init);
	CHECK(close(sockets[0]));
	CHECK(close(sockets[1]));
}

FN_TEST(pid_namespace_translates_unix_credentials)
{
	TEST_RES(run_child(check_socket_credentials), _ret == 0);
}
END_TEST()

static void *check_namespace_thread(void *unused)
{
	(void)unused;
	CHECK_WITH(getpid(), _ret == 1);
	CHECK_WITH(syscall(SYS_gettid), _ret > 1);
	return NULL;
}

static int cloned_init(void *unused)
{
	(void)unused;
	CHECK_WITH(getpid(), _ret == 1);
	CHECK_WITH(getppid(), _ret == 0);
	pthread_t thread;
	CHECK_WITH(pthread_create(&thread, NULL, check_namespace_thread, NULL),
		   _ret == 0);
	CHECK_WITH(pthread_join(thread, NULL), _ret == 0);
	return 0;
}

static void check_clone_and_threads(void)
{
	char *stack = malloc(1024 * 1024);
	CHECK_WITH(stack, _ret != NULL);
	pid_t child = CHECK(clone(cloned_init, stack + 1024 * 1024,
				  CLONE_NEWPID | SIGCHLD, NULL));
	wait_success(child);
	free(stack);
}

FN_TEST(pid_namespace_clone_and_threads)
{
	TEST_RES(run_child(check_clone_and_threads), _ret == 0);
}
END_TEST()

static void handle_signal(int number)
{
	(void)number;
}

static void check_signal_scope(void)
{
	pid_t outer = getpid();
	int outer_pidfd = CHECK(syscall(SYS_pidfd_open, outer, 0));
	CHECK(unshare(CLONE_NEWPID));
	pid_t init = CHECK(fork());
	if (init == 0) {
		CHECK_WITH(kill(outer, 0), _ret == -1 && errno == ESRCH);
		CHECK_WITH(syscall(SYS_tgkill, outer, outer, 0),
			   _ret == -1 && errno == ESRCH);
		CHECK_WITH(syscall(SYS_pidfd_open, outer, 0),
			   _ret == -1 && errno == ESRCH);
		CHECK_WITH(syscall(SYS_pidfd_send_signal, outer_pidfd, 0, NULL,
				   0),
			   _ret == -1 && errno == EINVAL);
		cpu_set_t mask;
		CHECK_WITH(sched_getaffinity(outer, sizeof(mask), &mask),
			   _ret == -1 && errno == ESRCH);
		CHECK(kill(1, SIGKILL));
		CHECK(kill(1, SIGTERM));
		CHECK(kill(1, SIGSTOP));
		CHECK_WITH(getpid(), _ret == 1);
		sigset_t signals;
		CHECK(sigemptyset(&signals));
		CHECK(sigaddset(&signals, SIGUSR1));
		CHECK(sigprocmask(SIG_BLOCK, &signals, NULL));
		struct sigaction action = { .sa_handler = handle_signal };
		CHECK(sigaction(SIGUSR1, &action, NULL));
		pid_t child = CHECK(fork());
		if (child == 0) {
			CHECK(kill(1, SIGUSR1));
			_exit(EXIT_SUCCESS);
		}
		CHECK_WITH(child, _ret == 2);
		siginfo_t info;
		struct timespec timeout = { .tv_sec = 5 };
		CHECK_WITH(sigtimedwait(&signals, &info, &timeout),
			   _ret == SIGUSR1 && info.si_pid == 2);
		CHECK(waitid(P_PID, child, &info, WEXITED));
		CHECK_WITH(info.si_pid, _ret == child &&
						info.si_code == CLD_EXITED &&
						info.si_status == 0);
		struct clone_args exit_signal_args = { .exit_signal = SIGKILL };
		pid_t notifier = CHECK(syscall(SYS_clone3, &exit_signal_args,
					       sizeof(exit_signal_args)));
		if (notifier == 0)
			_exit(EXIT_SUCCESS);
		int notifier_status;
		CHECK_WITH(waitpid(notifier, &notifier_status, __WCLONE),
			   _ret == notifier && WIFEXITED(notifier_status) &&
				   WEXITSTATUS(notifier_status) == 0);
		_exit(EXIT_SUCCESS);
	}
	wait_success(init);
	CHECK(close(outer_pidfd));
}

FN_TEST(pid_namespace_rejects_outer_identifiers_and_self_kill)
{
	TEST_RES(run_child(check_signal_scope), _ret == 0);
}
END_TEST()

static void check_ancestor_stop_and_resume(void)
{
	int gate[2];
	CHECK(pipe(gate));
	CHECK(unshare(CLONE_NEWPID));
	pid_t init = CHECK(fork());
	if (init == 0) {
		CHECK(close(gate[1]));
		char byte;
		CHECK_WITH(read(gate[0], &byte, 1), _ret == 1);
		_exit(EXIT_SUCCESS);
	}
	CHECK(close(gate[0]));
	CHECK(kill(init, SIGSTOP));
	int status;
	CHECK_WITH(waitpid(init, &status, WUNTRACED),
		   _ret == init && WIFSTOPPED(status) &&
			   WSTOPSIG(status) == SIGSTOP);
	CHECK(kill(init, SIGCONT));
	CHECK_WITH(waitpid(init, &status, WCONTINUED),
		   _ret == init && WIFCONTINUED(status));
	CHECK_WITH(write(gate[1], "r", 1), _ret == 1);
	wait_success(init);
	CHECK(close(gate[1]));
}

FN_TEST(pid_namespace_ancestor_can_stop_and_resume_init)
{
	TEST_RES(run_child(check_ancestor_stop_and_resume), _ret == 0);
}
END_TEST()

static void check_nested_namespaces(void)
{
	CHECK(unshare(CLONE_NEWPID));
	pid_t init = CHECK(fork());
	if (init == 0) {
		CHECK_WITH(getpid(), _ret == 1);
		CHECK(unshare(CLONE_NEWPID));
		CHECK_WITH(getpid(), _ret == 1);
		pid_t nested = CHECK(fork());
		if (nested == 0) {
			CHECK_WITH(getpid(), _ret == 1);
			CHECK_WITH(getppid(), _ret == 0);
			pid_t child = CHECK(fork());
			if (child == 0) {
				CHECK_WITH(getpid(), _ret == 2);
				CHECK_WITH(getppid(), _ret == 1);
				_exit(EXIT_SUCCESS);
			}
			CHECK_WITH(child, _ret == 2);
			wait_success(child);
			_exit(EXIT_SUCCESS);
		}
		CHECK_WITH(nested, _ret == 2);
		wait_success(nested);
		_exit(EXIT_SUCCESS);
	}
	wait_success(init);
}

FN_TEST(pid_namespace_translates_nested_identifiers)
{
	TEST_RES(run_child(check_nested_namespaces), _ret == 0);
}
END_TEST()

static void check_failed_first_clone(void)
{
	CHECK(unshare(CLONE_NEWPID));
	struct clone_args args = {
		.flags = CLONE_PIDFD,
		.pidfd = 1,
		.exit_signal = SIGCHLD,
	};
	long result = syscall(SYS_clone3, &args, sizeof(args));
	int failure = errno;
	if (result == 0)
		_exit(EXIT_FAILURE);
	CHECK_WITH(result, _ret == -1 && failure == EFAULT);
	CHECK_WITH(fork(), _ret == -1 && errno == ENOMEM);
}

FN_TEST(pid_namespace_closes_after_unpublished_init_failure)
{
	TEST_RES(run_child(check_failed_first_clone), _ret == 0);
}
END_TEST()

static void check_invalid_clone_arguments(void)
{
	CHECK(unshare(CLONE_NEWPID));
	struct clone_args args = { .flags = CLONE_THREAD };
	CHECK_WITH(syscall(SYS_clone3, &args, sizeof(args)),
		   _ret == -1 && errno == EINVAL);
	args.flags = CLONE_THREAD | CLONE_VM | CLONE_SIGHAND | CLONE_NEWPID;
	CHECK_WITH(syscall(SYS_clone3, &args, sizeof(args)),
		   _ret == -1 && errno == EINVAL);
	pid_t init = CHECK(fork());
	if (init == 0) {
		CHECK_WITH(getpid(), _ret == 1);
		_exit(EXIT_SUCCESS);
	}
	wait_success(init);
}

FN_TEST(pid_namespace_validates_clone_before_allocating_init)
{
	TEST_RES(run_child(check_invalid_clone_arguments), _ret == 0);
}
END_TEST()

static void check_namespace_permissions(void)
{
	CHECK(setgid(65534));
	CHECK(setuid(65534));
	CHECK_WITH(unshare(CLONE_NEWPID), _ret == -1 && errno == EPERM);
}

FN_TEST(pid_namespace_requires_administrative_capability)
{
	TEST_RES(run_child(check_namespace_permissions), _ret == 0);
}
END_TEST()

static void check_setns(void)
{
	private_proc();
	int initial_namespace =
		CHECK(open("/proc/self/ns/pid", O_RDONLY | O_CLOEXEC));
	int ready[2];
	int gate[2];
	CHECK(pipe(ready));
	CHECK(pipe(gate));
	CHECK(unshare(CLONE_NEWPID));
	struct stat metadata;
	CHECK(lstat("/proc/self/ns/pid_for_children", &metadata));
	char link[128];
	CHECK_WITH(readlink("/proc/self/ns/pid_for_children", link,
			    sizeof(link)),
		   _ret == -1 && errno == ENOENT);
	pid_t init = CHECK(fork());
	if (init == 0) {
		CHECK_WITH(setns(initial_namespace, CLONE_NEWPID),
			   _ret == -1 && errno == EINVAL);
		CHECK_WITH(write(ready[1], "r", 1), _ret == 1);
		char byte;
		CHECK_WITH(read(gate[0], &byte, 1), _ret == 1);
		_exit(EXIT_SUCCESS);
	}
	char byte;
	CHECK_WITH(read(ready[0], &byte, 1), _ret == 1);
	CHECK_WITH(readlink("/proc/self/ns/pid_for_children", link,
			    sizeof(link)),
		   _ret > 0);
	int child_namespace = CHECK(
		open("/proc/self/ns/pid_for_children", O_RDONLY | O_CLOEXEC));
	CHECK(close(child_namespace));
	CHECK_WITH(write(gate[1], "r", 1), _ret == 1);
	wait_success(init);
	CHECK_WITH(readlink("/proc/self/ns/pid_for_children", link,
			    sizeof(link)),
		   _ret > 0);
	CHECK(setns(initial_namespace, CLONE_NEWPID));
	child_namespace = CHECK(
		open("/proc/self/ns/pid_for_children", O_RDONLY | O_CLOEXEC));
	struct stat restored;
	CHECK(fstat(initial_namespace, &metadata));
	CHECK(fstat(child_namespace, &restored));
	CHECK_WITH(restored.st_ino, _ret == metadata.st_ino);
	CHECK(close(child_namespace));
	pid_t caller = getpid();
	pid_t child = CHECK(fork());
	if (child == 0) {
		CHECK_WITH(getppid(), _ret == caller);
		_exit(EXIT_SUCCESS);
	}
	wait_success(child);
	CHECK(close(initial_namespace));
	CHECK(close(ready[0]));
	CHECK(close(ready[1]));
	CHECK(close(gate[0]));
	CHECK(close(gate[1]));
}

FN_TEST(pid_namespace_setns_changes_future_children)
{
	TEST_RES(run_child(check_setns), _ret == 0);
}
END_TEST()

static void check_init_teardown(void)
{
	int ready[2];
	int lifetime[2];
	CHECK(pipe(ready));
	CHECK(pipe(lifetime));
	CHECK(unshare(CLONE_NEWPID));
	pid_t init = CHECK(fork());
	if (init == 0) {
		CHECK(close(ready[0]));
		CHECK(close(lifetime[0]));
		int adopted[2];
		CHECK(pipe(adopted));
		pid_t child = CHECK(fork());
		if (child == 0) {
			pid_t grandchild = CHECK(fork());
			if (grandchild == 0) {
				char byte;
				CHECK_WITH(read(adopted[0], &byte, 1),
					   _ret == 1);
				CHECK_WITH(getppid(), _ret == 1);
				CHECK_WITH(write(ready[1], "r", 1), _ret == 1);
				for (;;)
					pause();
			}
			_exit(EXIT_SUCCESS);
		}
		CHECK(close(lifetime[1]));
		wait_success(child);
		CHECK_WITH(write(adopted[1], "a", 1), _ret == 1);
		for (;;)
			pause();
	}
	CHECK(close(ready[1]));
	CHECK(close(lifetime[1]));
	char byte;
	CHECK_WITH(read(ready[0], &byte, 1), _ret == 1);
	int pidfd = CHECK(syscall(SYS_pidfd_open, init, 0));
	CHECK(kill(init, SIGKILL));
	struct pollfd exited = { .fd = pidfd, .events = POLLIN };
	CHECK_WITH(poll(&exited, 1, 5000),
		   _ret == 1 && (exited.revents & POLLIN));
	struct pollfd resources = { .fd = lifetime[0], .events = POLLIN };
	CHECK_WITH(poll(&resources, 1, 0),
		   _ret == 1 && (resources.revents & POLLHUP));
	int status;
	CHECK_WITH(waitpid(init, &status, 0),
		   _ret == init && WIFSIGNALED(status) &&
			   WTERMSIG(status) == SIGKILL);
	CHECK_WITH(fork(), _ret == -1 && errno == ENOMEM);
	CHECK(close(pidfd));
	CHECK(close(ready[0]));
	CHECK(close(lifetime[0]));
}

FN_TEST(pid_namespace_init_exit_closes_descendant_resources)
{
	TEST_RES(run_child(check_init_teardown), _ret == 0);
}
END_TEST()

static void clone_until_init_exits(int namespace, int ready, int done)
{
	CHECK(setns(namespace, CLONE_NEWPID));
	for (int iteration = 0; iteration < 128; iteration++) {
		pid_t child = fork();
		int failure = errno;
		if (child == 0) {
			CHECK_WITH(getpid(), _ret > 1);
			CHECK_WITH(getppid(), _ret == 0);
			_exit(EXIT_SUCCESS);
		}
		if (child < 0) {
			CHECK_WITH(failure, _ret == ENOMEM && iteration > 0);
			return;
		}
		if (iteration == 0)
			CHECK_WITH(write(ready, "w", 1), _ret == 1);
		int status;
		CHECK_WITH(waitpid(child, &status, 0),
			   _ret == child && ((WIFEXITED(status) &&
					      WEXITSTATUS(status) == 0) ||
					     (WIFSIGNALED(status) &&
					      WTERMSIG(status) == SIGKILL)));
	}
	char byte;
	CHECK_WITH(read(done, &byte, 1), _ret == 0);
	CHECK_WITH(fork(), _ret == -1 && errno == ENOMEM);
}

static void check_concurrent_clones(void)
{
	private_proc();
	int initial_namespace =
		CHECK(open("/proc/self/ns/pid", O_RDONLY | O_CLOEXEC));
	int ready[2];
	int gate[2];
	int done[2];
	CHECK(pipe(ready));
	CHECK(pipe(gate));
	CHECK(pipe(done));
	CHECK(unshare(CLONE_NEWPID));
	pid_t init = CHECK(fork());
	if (init == 0) {
		CHECK_WITH(write(ready[1], "i", 1), _ret == 1);
		char byte;
		CHECK_WITH(read(gate[0], &byte, 1), _ret == 1);
		_exit(EXIT_SUCCESS);
	}
	char byte;
	CHECK_WITH(read(ready[0], &byte, 1), _ret == 1 && byte == 'i');
	char path[128];
	CHECK_WITH(snprintf(path, sizeof(path), "/proc/%d/ns/pid", init),
		   _ret > 0 && _ret < sizeof(path));
	int namespace = CHECK(open(path, O_RDONLY | O_CLOEXEC));
	CHECK(setns(initial_namespace, CLONE_NEWPID));
	pid_t workers[2];
	for (int i = 0; i < 2; i++) {
		workers[i] = CHECK(fork());
		if (workers[i] == 0) {
			CHECK(close(done[1]));
			clone_until_init_exits(namespace, ready[1], done[0]);
			_exit(EXIT_SUCCESS);
		}
	}
	for (int i = 0; i < 2; i++)
		CHECK_WITH(read(ready[0], &byte, 1), _ret == 1 && byte == 'w');
	CHECK(kill(init, SIGKILL));
	int status;
	CHECK_WITH(waitpid(init, &status, 0),
		   _ret == init && WIFSIGNALED(status) &&
			   WTERMSIG(status) == SIGKILL);
	CHECK(close(done[1]));
	for (int i = 0; i < 2; i++)
		wait_success(workers[i]);
	CHECK(close(initial_namespace));
	CHECK(close(namespace));
	CHECK(close(ready[0]));
	CHECK(close(ready[1]));
	CHECK(close(gate[0]));
	CHECK(close(gate[1]));
	CHECK(close(done[0]));
}

FN_TEST(pid_namespace_closes_admission_during_concurrent_clones)
{
	TEST_RES(run_child(check_concurrent_clones), _ret == 0);
}
END_TEST()
