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
#	device and partition 2 is the cache. Separate log devices can be part of
#	two different pools
#
# STRATEGY:
#	1. Get a list of devices that are available
#	2. For each device partition it into two parts such that 1/4 of the
#	   disk size is allocated to the log device & remaining 3/4 to the
#	   zcache device
#	3. Stop the zfs-object-agent and add the list of devices that were
#	   partitioned to be used as zettacache devices
#	4. Start the zfs-object-agent
#	5. Create two separate pools using two different slog devices
#	6. Verify that the pool was created
#	7. Verify that the log devices are still online for both pool
#

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Test not applicable for block based run"
fi

function cleanup
{
	poolexists $TESTPOOL && destroy_pool $TESTPOOL
	poolexists $TESTPOOL1 && destroy_pool $TESTPOOL1
	invalidate_zcache
}

log_assert "Verify that the shared device(s) are partitioned into two parts." \
	" The first part should be usable as slog device and the second as zettacache" \
	" device. We should be able to create two separate pools using different slog" \
	" devices"

log_onexit cleanup

typeset device_array=($AVAILABLE_DEVICES)

if [ ${#device_array[@]} -lt 2 ]; then
	log_unsupported "Minimum 2 devices required for this test"
fi

typeset slog_devices_array=($SLOG_DEVICES)

# Create first pool with 1 log device
log_must create_pool -p $TESTPOOL -l "log ${slog_devices_array[0]}"
log_must poolexists $TESTPOOL
log_must zfs create $TESTPOOL/$TESTFS

# Create second pool with remaining log devices
log_must create_pool -p $TESTPOOL1 -l "log ${slog_devices_array[*]:1}"
log_must poolexists $TESTPOOL1
log_must zfs create $TESTPOOL1/$TESTFS

for pool in $TESTPOOL $TESTPOOL1; do
	log_must dd if=/dev/zero of=/$pool/$TESTFS/sync \
		conv=fdatasync,fsync bs=1M count=100
done

for pool in $TESTPOOL $TESTPOOL1; do
	log_must zpool sync $pool
done

verify_slog_devices_are_online $TESTPOOL ${slog_devices_array[0]}
verify_slog_devices_are_online $TESTPOOL1 ${slog_devices_array[@]:1}

log_pass "Partitioning the shared device and using it as a slog device" \
	" succeeded with multiple test pool(s)"
