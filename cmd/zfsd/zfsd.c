/*
 * This file and its contents are supplied under the terms of the
 * Common Development and Distribution License ("CDDL"), version 1.0.
 * You may only use this file in accordance with the terms of version
 * 1.0 of the CDDL.
 *
 * A full copy of the text of the CDDL should have accompanied this
 * source.  A copy of the CDDL is also available via the Internet at
 * http://www.illumos.org/license/CDDL.
 */

/*
 * Copyright (c) 2013, 2020 by Delphix. All rights reserved.
 */

/*
 * XXX - Big Theory Statement?
 */

#include <stdio.h>
#include <stdlib.h>
#include <sys/zfs_context.h>
#include <sys/types.h>
#include <fcntl.h>
#include <sys/zfs_ioctl.h>
#include <signal.h>

int
main(int argc, char *argv[])
{
	char c;

	extern const char *spa_config_path;

	if (getenv(ZFS_SOCKET_ENVVAR) == NULL) {
		fprintf(stderr, "%s not set in env\n", ZFS_SOCKET_ENVVAR);
		exit(1);
	}

	spa_config_path = "/var/tmp/zfsd.cache";

	while ((c = getopt(argc, argv, "c:")) != -1) {
		switch (c) {
		case 'c':
			spa_config_path = optarg;
			break;
		default:
			abort();
			break;
		}
	}
	argc -= optind;
	argv += optind;

	kernel_init(SPA_MODE_READ | SPA_MODE_WRITE);
	sigignore(SIGPIPE);

	while (1) {
		sleep(100);
	}

	return (0);
}
