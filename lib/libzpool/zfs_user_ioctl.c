/*
 * CDDL HEADER START
 *
 * The contents of this file are subject to the terms of the
 * Common Development and Distribution License (the "License").
 * You may not use this file except in compliance with the License.
 *
 * You can obtain a copy of the license at usr/src/OPENSOLARIS.LICENSE
 * or http://www.opensolaris.org/os/licensing.
 * See the License for the specific language governing permissions
 * and limitations under the License.
 *
 * When distributing Covered Code, include this CDDL HEADER in each
 * file and include the License file at usr/src/OPENSOLARIS.LICENSE.
 * If applicable, add the following below this CDDL HEADER, with the
 * fields enclosed by brackets "[]" replaced with your own identifying
 * information: Portions Copyright [yyyy] [name of copyright owner]
 *
 * CDDL HEADER END
 */

/*
 * Copyright (c) 2013, 2020 by Delphix. All rights reserved.
 */

#include <libzfs.h>

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/un.h>

#include <sys/zfs_ioctl.h>


/*
 * XXX: Big Theory Statement?
 */

static pthread_key_t pid_key;
static pthread_key_t ioctl_key;

typedef struct conn_arg {
	int conn_fd;
} conn_arg_t;

int
ioctl_recv(int size, void *dst)
{
	conn_arg_t *ca = pthread_getspecific(ioctl_key);

	while (size > 0) {
		int recvd = recv(ca->conn_fd, dst, size, 0);
		if (recvd == -1)
			return (errno);
		if (recvd == 0)
			return (EINVAL);
		size -= recvd;
		dst = (char *)dst + recvd;
	}
	return (0);
}

int
ioctl_send(int size, const void *data)
{
	conn_arg_t *ca = pthread_getspecific(ioctl_key);

	while (size > 0) {
		int sent = send(ca->conn_fd, data, size, 0);
		if (sent == -1)
			return (errno);
		if (sent == 0)
			return (EINVAL);
		size -= sent;
		data = (const char *)data + sent;
	}
	return (0);
}

int
ioctl_sendmsg(const zfs_ioctl_msg_t *msg, int payload_len, const void *payload)
{
	int error;

	error = ioctl_send(sizeof (*msg), msg);
	if (error != 0)
		return (error);

	if (payload_len != 0)
		error = ioctl_send(payload_len, payload);
	return (error);
}

int
ioctl_recvmsg(zfs_ioctl_msg_t *msg)
{
	return (ioctl_recv(sizeof (*msg), msg));
}

int
ddi_copyin(const void *src, void *dst, size_t size, int flag)
{
	int error;
	zfs_ioctl_msg_t msg = { 0 };

	printf("copyin %lu bytes from %p\n", size, src);

	msg.zim_type = ZIM_COPYIN;
	msg.zim_u.zim_copyin.zim_address = (uintptr_t)src;
	msg.zim_u.zim_copyin.zim_len = size;
	error = ioctl_sendmsg(&msg, 0, NULL);
	if (error != 0)
		return (error);

	error = ioctl_recvmsg(&msg);
	if (error != 0)
		return (error);

	ASSERT3U(msg.zim_type, ==, ZIM_COPYIN_RESPONSE);
	if (msg.zim_type != ZIM_COPYIN_RESPONSE)
		return (EINVAL);
	if (msg.zim_u.zim_copyin_response.zim_errno != 0)
		return (msg.zim_u.zim_copyin_response.zim_errno);

	error = ioctl_recv(size, dst);
	printf("errno=%u\n", error);
	return (error);
}

/* XXX [mahrens]: copied; need copyrights */
const struct ioc {
	uint_t	code;
	const char *name;
	const char *datastruct;
} iocnames[] = {
	/* ZFS ioctls */
	{ (uint_t)ZFS_IOC_POOL_CREATE,		"ZFS_IOC_POOL_CREATE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_DESTROY,		"ZFS_IOC_POOL_DESTROY",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_IMPORT,		"ZFS_IOC_POOL_IMPORT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_EXPORT,		"ZFS_IOC_POOL_EXPORT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_CONFIGS,		"ZFS_IOC_POOL_CONFIGS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_STATS,		"ZFS_IOC_POOL_STATS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_TRYIMPORT,	"ZFS_IOC_POOL_TRYIMPORT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_SCAN,		"ZFS_IOC_POOL_SCAN",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_FREEZE,		"ZFS_IOC_POOL_FREEZE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_UPGRADE,		"ZFS_IOC_POOL_UPGRADE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_GET_HISTORY,	"ZFS_IOC_POOL_GET_HISTORY",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_ADD,		"ZFS_IOC_VDEV_ADD",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_REMOVE,		"ZFS_IOC_VDEV_REMOVE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_SET_STATE,	"ZFS_IOC_VDEV_SET_STATE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_ATTACH,		"ZFS_IOC_VDEV_ATTACH",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_DETACH,		"ZFS_IOC_VDEV_DETACH",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_SETPATH,		"ZFS_IOC_VDEV_SETPATH",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_SETFRU,		"ZFS_IOC_VDEV_SETFRU",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_OBJSET_STATS,		"ZFS_IOC_OBJSET_STATS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_OBJSET_ZPLPROPS,	"ZFS_IOC_OBJSET_ZPLPROPS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_DATASET_LIST_NEXT,	"ZFS_IOC_DATASET_LIST_NEXT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SNAPSHOT_LIST_NEXT,	"ZFS_IOC_SNAPSHOT_LIST_NEXT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SET_PROP,		"ZFS_IOC_SET_PROP",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_CREATE,		"ZFS_IOC_CREATE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_DESTROY,		"ZFS_IOC_DESTROY",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_ROLLBACK,		"ZFS_IOC_ROLLBACK",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_RENAME,		"ZFS_IOC_RENAME",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_RECV,			"ZFS_IOC_RECV",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SEND,			"ZFS_IOC_SEND",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_INJECT_FAULT,		"ZFS_IOC_INJECT_FAULT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_CLEAR_FAULT,		"ZFS_IOC_CLEAR_FAULT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_INJECT_LIST_NEXT,	"ZFS_IOC_INJECT_LIST_NEXT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_ERROR_LOG,		"ZFS_IOC_ERROR_LOG",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_CLEAR,		"ZFS_IOC_CLEAR",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_PROMOTE,		"ZFS_IOC_PROMOTE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SNAPSHOT,		"ZFS_IOC_SNAPSHOT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_DSOBJ_TO_DSNAME,	"ZFS_IOC_DSOBJ_TO_DSNAME",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_OBJ_TO_PATH,		"ZFS_IOC_OBJ_TO_PATH",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_SET_PROPS,	"ZFS_IOC_POOL_SET_PROPS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_GET_PROPS,	"ZFS_IOC_POOL_GET_PROPS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SET_FSACL,		"ZFS_IOC_SET_FSACL",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_GET_FSACL,		"ZFS_IOC_GET_FSACL",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SHARE,		"ZFS_IOC_SHARE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_INHERIT_PROP,		"ZFS_IOC_INHERIT_PROP",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SMB_ACL,		"ZFS_IOC_SMB_ACL",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_USERSPACE_ONE,	"ZFS_IOC_USERSPACE_ONE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_USERSPACE_MANY,	"ZFS_IOC_USERSPACE_MANY",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_USERSPACE_UPGRADE,	"ZFS_IOC_USERSPACE_UPGRADE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_HOLD,			"ZFS_IOC_HOLD",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_RELEASE,		"ZFS_IOC_RELEASE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_GET_HOLDS,		"ZFS_IOC_GET_HOLDS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_OBJSET_RECVD_PROPS,	"ZFS_IOC_OBJSET_RECVD_PROPS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_VDEV_SPLIT,		"ZFS_IOC_VDEV_SPLIT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_NEXT_OBJ,		"ZFS_IOC_NEXT_OBJ",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_DIFF,			"ZFS_IOC_DIFF",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_TMP_SNAPSHOT,		"ZFS_IOC_TMP_SNAPSHOT",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_OBJ_TO_STATS,		"ZFS_IOC_OBJ_TO_STATS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SPACE_WRITTEN,	"ZFS_IOC_SPACE_WRITTEN",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_DESTROY_SNAPS,	"ZFS_IOC_DESTROY_SNAPS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_REGUID,		"ZFS_IOC_POOL_REGUID",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_POOL_REOPEN,		"ZFS_IOC_POOL_REOPEN",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SEND_PROGRESS,	"ZFS_IOC_SEND_PROGRESS",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_LOG_HISTORY,		"ZFS_IOC_LOG_HISTORY",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SEND_NEW,		"ZFS_IOC_SEND_NEW",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_SEND_SPACE,		"ZFS_IOC_SEND_SPACE",
		"zfs_cmd_t" },
	{ (uint_t)ZFS_IOC_CLONE,		"ZFS_IOC_CLONE",
		"zfs_cmd_t" },
};

void
show_zfs_ioc(long addr)
{
	// XXX: -Werror=frame-larger-than 4K
	//      skipped code
	// --- skipped more
	// 	zfs_cmd_t zc;

	// 	if (ddi_copyin((void *)addr, &zc, sizeof (zc), 0) != 0) {
	// 		(void) printf(" zfs_ioctl read failed\n");
	// 		return;
	// 	}

	// 	if (zc.zc_name[0])
	// 		(void) printf("    zc_name=%s\n", zc.zc_name);
	// 	if (zc.zc_value[0])
	// 		(void) printf("    zc_value=%s\n", zc.zc_value);
	// 	if (zc.zc_string[0])
	// 		(void) printf("    zc_string=%s\n", zc.zc_string);
	// 	if (zc.zc_guid != 0) {
	// 		(void) printf("    zc_guid=%llu\n",
	// 		    (u_longlong_t)zc.zc_guid);
	// 	}
	// 	if (zc.zc_cookie != 0) {
	// 		(void) printf("    zc_cookie=%llu\n",
	// 		    (u_longlong_t)zc.zc_cookie);
	// 	}
	// 	if (zc.zc_objset_type != 0) {
	// 		(void) printf("    zc_objset_type=%llu\n",
	// 		    (u_longlong_t)zc.zc_objset_type);
	// 	}
	// 	if (zc.zc_obj != 0) {
	// 		(void) printf("    zc_obj=%llu\n",
	// 		    (u_longlong_t)zc.zc_obj);
	// 	}
}

const char *
ioc2name(int ioc)
{
	for (int i = 0; i < sizeof (iocnames) / sizeof (iocnames[0]); i++)
		if (iocnames[i].code == ioc)
			return (iocnames[i].name);
	return ("unknown");
}

static void *
handle_connection(void *argp)
{
	conn_arg_t *arg = argp;
	int conn_fd = arg->conn_fd;
	int error;

	VERIFY0(pthread_setspecific(ioctl_key, arg));

	while (B_TRUE) {
		zfs_ioctl_msg_t msg;
		int ioctl;
		uint64_t arg;
		error = ioctl_recvmsg(&msg);


		if (error != 0) {
			perror("ioctl_recvmsg failed");
			return (NULL);
		}

		if (msg.zim_type != ZIM_IOCTL) {
			printf("unexpected message received (type %u)\n",
			    msg.zim_type);
			break;
		}

		ioctl = msg.zim_u.zim_ioctl.zim_ioctl;
		arg = msg.zim_u.zim_ioctl.zim_cmd;

		printf("zfsdev_ioctl(%s %lx)\n",
		    ioc2name(ioctl), arg);
		show_zfs_ioc(arg);

		msg.zim_type = ZIM_IOCTL_RESPONSE;
		msg.zim_u.zim_ioctl_response.zim_errno = zfsdev_ioctl(0,
		    ioctl, arg, 0, NULL, NULL);
		msg.zim_u.zim_ioctl_response.zim_retval =
		    (msg.zim_u.zim_ioctl_response.zim_errno == 0) ? 0 : -1;

		printf("errno = %u\n", msg.zim_u.zim_ioctl_response.zim_errno);

		error = ioctl_sendmsg(&msg, 0, NULL);
		if (error != 0) {
			perror("sendmsg failed");
			break;
		}
	}
	(void) close(conn_fd);
	free(arg);
	return (NULL);
}

static void *
accept_connection(void *argp)
{
	int sock_fd = (uintptr_t)argp;
	int conn_fd;
	struct sockaddr_un address;
	socklen_t socklen = sizeof(address);

	/* XXX [mahrens] - kick off a thread for this */
	while ((conn_fd = accept(sock_fd, (struct sockaddr *)&address,
	    &socklen)) >= 0) {
		pthread_t tid;

		conn_arg_t *ca = malloc(sizeof (conn_arg_t));
		ca->conn_fd = conn_fd;
		pthread_create(&tid, NULL, handle_connection, ca);
	}

	if (conn_fd < 0)
		perror("accept failed");

	return (NULL);
}

void
zfs_user_ioctl_init(void)
{
	int sock_fd;
	struct sockaddr_un address = { 0 };
	char *socket_name;
	pthread_t tid;
	int error;

	// XXX [NEXT]:zfs_ioctl_init();

	socket_name = getenv(ZFS_SOCKET_ENVVAR);
	if (socket_name == NULL)
		return;

        error = pthread_key_create(&pid_key, NULL);
        if (error != 0) {
		perror("pthread_key_create");
		abort();
        }

        error = pthread_key_create(&ioctl_key, NULL);
        if (error != 0) {
		perror("pthread_key_create");
		abort();
        }

	/*
	 * XXX: Explain why
	 * XXX: sigignore() may be deprecated.
	 *      Use whatever the new thing is.
	 */
	sigignore(SIGPIPE);

	if ((sock_fd = socket(PF_UNIX, SOCK_STREAM, 0)) < 0) {
		perror("socket failed");
		exit(1);
	}

	address.sun_family = AF_UNIX;
	// XXX: double-check format issue (see diff)
	snprintf(address.sun_path, sizeof(address.sun_path), "%s",
	    socket_name);

	(void) unlink(socket_name);
	if (bind(sock_fd, (struct sockaddr *)&address,
	    sizeof(struct sockaddr_un)) != 0) {
		perror("bind failed");
		exit(1);
	}

	if (listen(sock_fd, 2) != 0) {
		perror("listen failed");
		exit(1);
	}

	pthread_create(&tid, NULL, accept_connection,
	    (void*)(uintptr_t)sock_fd);
}
