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
 * Copyright (c) 2021 by Delphix. All rights reserved.
 */

#include <stdio.h>
#include <sys/un.h>
#include <sys/socket.h>
#include <sys/zfs_context.h>
#include <object_agent.h>
#ifdef HAVE_LIBZOA
#include <libzoa.h>
#endif

#ifdef HAVE_LIBZOA

#define	MAX_ZOA_WAIT 300

static char zoa_sock_dir[] = "/tmp/zoa.sock.XXXXXX";
static char zoa_log_file[PATH_MAX] = "/tmp/zoa.log";

/*
 * Wait until the specified unix socket starts accepting connections.
 */
static int
zoa_socket_init_wait(char *zoa_sock_str)
{
	struct sockaddr_un zoa_socket;
	zoa_socket.sun_family = AF_UNIX;
	(void) strncpy(zoa_socket.sun_path, zoa_sock_str,
	    sizeof (zoa_socket.sun_path));
	zoa_socket.sun_path[sizeof (zoa_socket.sun_path) - 1] = '\0';

	for (int i = 0; i < MAX_ZOA_WAIT; i++) {
		int sock = socket(AF_UNIX, SOCK_STREAM, 0);
		if (sock < 0) {
			(void) fprintf(stderr, "failed to create socket %s.\n",
			    zoa_sock_str);
			return (-1);
		}
		int rc = connect(sock, &zoa_socket,
		    sizeof (struct sockaddr_un));
		close(sock);

		if (rc == 0) {
			return (0);
		}
		sleep(1);
	}

	return (-1);
}

/*
 * Wait for zfs object agent to initialize and start accepting requests.
 */
static int
zoa_init_wait(char *zoa_sock_dir)
{
	char zoa_sock[PATH_MAX];

	snprintf(zoa_sock, PATH_MAX, "%s/zfs_root_socket", zoa_sock_dir);
	if (zoa_socket_init_wait(zoa_sock) != 0) {
		return (-1);
	}

	snprintf(zoa_sock, PATH_MAX, "%s/zfs_public_socket", zoa_sock_dir);
	if (zoa_socket_init_wait(zoa_sock) != 0) {
		return (-1);
	}

	return (0);
}

static void
zoa_thread(void *arg)
{
	set_object_agent_sock_dir(zoa_sock_dir);
	VERIFY0(libzoa_init(zoa_sock_dir, zoa_log_file, NULL, (void **)arg));
}
#endif

/*
 * Initialize the zfs object agent (libzoa) is a seperate thread and wait for
 * initialization to finish.
 */
int
start_zfs_object_agent(char *logfile, void **handle)
{
#ifdef HAVE_LIBZOA
	(void) strlcpy(zoa_log_file, logfile, sizeof (zoa_log_file));
	char *dir = mkdtemp(zoa_sock_dir);
	ASSERT3S(dir, !=, NULL);

	thread_create(NULL, 0, zoa_thread, handle, 0, NULL,
	    TS_RUN | TS_JOINABLE, defclsyspri);

	return (zoa_init_wait(zoa_sock_dir));
#else
	fatal(0, "libzoa support missing.");
#endif
}
