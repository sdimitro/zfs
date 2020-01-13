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

static void *
handle_connection(void *argp)
{
	// XXX [NEXT]
	printf("BOOM!\n");
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
