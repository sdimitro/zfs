#!/bin/ksh -p
#
# CDDL HEADER START
#
# The contents of this file are subject to the terms of the
# Common Development and Distribution License (the "License").
# You may not use this file except in compliance with the License.
#
# You can obtain a copy of the license at usr/src/OPENSOLARIS.LICENSE
# or http://www.opensolaris.org/os/licensing.
# See the License for the specific language governing permissions
# and limitations under the License.
#
# When distributing Covered Code, include this CDDL HEADER in each
# file and include the License file at usr/src/OPENSOLARIS.LICENSE.
# If applicable, add the following below this CDDL HEADER, with the
# fields enclosed by brackets "[]" replaced with your own identifying
# information: Portions Copyright [yyyy] [name of copyright owner]
#
# CDDL HEADER END
#

#
# Copyright (c) 2022 by Delphix. All rights reserved.
#

. $STF_SUITE/include/libtest.shlib
. $STF_SUITE/include/object_store.shlib
. $STF_SUITE/tests/functional/slog/slog.kshlib
. $STF_SUITE/tests/functional/zcache_slog_sharing/zcache_slog_sharing.kshlib


#
# DESCRIPTION:
#	Verify that the shared device(s) are partitioned and partition 1 is log
#	device and partition 2 is the cache. The log device can be added after
#	the pool creation
#
# STRATEGY:
#	1. Get a list of devices that are available
#	2. For each device partition it into two parts such that 1/4 of the
#	   disk size is allocated to the log device & remaining 3/4 to the
#	   zcache device
#	3. Stop the zfs-object-agent and add the list of devices that were
#	   partitioned to be used as zettacache devices
#	4. Start the zfs-object-agent
#	5. Create a pool without log devices
#	6. Add the log devices with zpool add operation
#	7. Verify that the log devices were correctly configured
#

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Test not applicable for block based run"
fi

function cleanup
{
	poolexists $TESTPOOL && destroy_pool $TESTPOOL
	invalidate_zcache
}

log_assert "Verify that the shared device(s) are partitioned into two parts." \
	" The first part should be usable as slog device and the second as zettacache" \
	" device. The slog devices are added after the pool creation"

log_onexit cleanup

log_must create_pool -p $TESTPOOL
log_must poolexists $TESTPOOL

log_must zpool add $TESTPOOL log $SLOG_DEVICES
log_must zfs create $TESTPOOL/$TESTFS

# Do some sync write
log_must dd if=/dev/zero of=/$TESTPOOL/$TESTFS/sync \
    conv=fdatasync,fsync bs=1M count=100

log_must zpool sync $TESTPOOL

verify_slog_devices_are_online $TESTPOOL $SLOG_DEVICES

log_pass "Partitioning the shared device and using it as a slog device" \
	" succeeded with zpool add operation"
