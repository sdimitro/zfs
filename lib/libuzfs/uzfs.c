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
 * Copyright (c) 2020 by Delphix. All rights reserved.
 */

/*
 * This is an experimental library that leverages libzpool
 * to bring ZFS to the userland. There are 4 approaches to
 * this experiment that will take place though this library
 * that could potentially be used in complement to each
 * other. These approaches are:
 *
 * 1] UNIX Sockets -
 *      This is the most limited solution as it would be
 *      hard(impossible?) to incorporate anything outside
 *      of ioctls (e.g. open(), read(), ...) cleanly with
 *      userland applications like common UNIX utilities
 *      without invasive runtime modifications. The cool
 *      thing about it is the barrier of entry and the
 *      fact that it doesn't have external dependecies.
 *
 * 2] FUSE -
 *      This is the most generic and complete approach
 *      but probably one of the least performant.
 *
 * 3] ZUFS -
 *      This is fairly-new and promising solution where
 *      we supposingly get the completeness of the FUSE
 *      solution but with better performance. Unfortunately
 *      for now it is the least explored approach.
 *
 * 4] syscall_intercept -
 *      The idea here is to smartly intercept all syscalls
 *      and direct them to our userland ZFS library. While
 *      potentially limited to one applications/process at
 *      a time, this can be the most performant of all the
 *      solutions (e.g. could work best for appliances).
 *
 * - Serapheim
 */

#include <libsyscall_intercept_hook_point.h>

#include <errno.h>
#include <syscall.h>

#include <sys/zfs_context.h>
#include <sys/zfs_ioctl.h>
#include <sys/zfs_ioctl_impl.h>

typedef enum intercept_code {
	INTERCEPTED = 0,
	SEND_TO_KERNEL = 1,
} intercept_code_t;

static int
uzfs_hook(long syscall_number,
	long arg0, long arg1, long arg2,
	long arg3, long arg4, long arg5,
	long *result)
{
	(void) arg0;
	(void) arg2;
	(void) arg3;
	(void) arg4;
	(void) arg5;

	switch (syscall_number) {
	case SYS_ioctl:
	{
		/*
		 * This is a hacky check but we basically filter out
		 * on non-ZFS ioctl numbers. This could be problematic
		 * as our process could potentially issue ioctls that
		 * are within the ZFS ioctl range to another device
		 * at which point things would go bad. This should work
		 * for now though. A more correct solution would be to
		 * use readlink() on /proc/self/fd/{arg0} and ensure
		 * that this is the /dev/zfs special device.
		 */
		uint_t cmd = arg1;
		if (cmd < ZFS_IOC_FIRST || cmd > ZFS_IOC_LAST)
			return SEND_TO_KERNEL;

		uint_t vecnum = cmd - ZFS_IOC_FIRST;
		zfs_cmd_t *zc = (void *)(uintptr_t)arg2;
		*result = -zfsdev_ioctl_common(vecnum, zc, 0);

		return INTERCEPTED;
	}
	default:
		return SEND_TO_KERNEL; /* don't intercept; let it go to the kernel */
	}
}

static __attribute__((constructor)) void
library_init(void)
{
	intercept_hook_point = &uzfs_hook;
	kernel_init(SPA_MODE_READ | SPA_MODE_WRITE);
}

static __attribute__((destructor)) void
library_fini(void)
{
	kernel_fini();
}
