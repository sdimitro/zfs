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


#
# DESCRIPTION:
#	Verify that restarting the zfs object agent successfully resumes zpool
#	destroy operation
#
# STRATEGY:
#	1. Create an object store based pool
#	2. Set object_deletion_batch_size to 10 (default is 1000), so that pool
#	   deletion in object store proceeds slowly and we are able to test resume
#	   operation
#	3. Destroy the pool
#	4. Verify that the pool is in DESTROYING state
#	5. Stop the agent
#	6. Set object_deletion_batch_size back to 1000, so that the deletion
#	   progresses fast
#	7. Start the agent
#	8. Verify the pool is destroyed successfully
#	9. Verify the object store space is freed up
#

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Test not applicable for block based run"
fi

function cleanup
{
	poolexists $TESTPOOL && destroy_pool $TESTPOOL
	zpool status --clear-destroyed
}

log_assert "Verify that restarting the zfs object agent successfully resumes" \
	" zpool destroy operation"

log_onexit cleanup

create_pool -p $TESTPOOL
typeset guid=$(get_object_store_pool_guid $TESTPOOL)

# populate the zpool with 1 GB data
sudo dd if=/dev/zero of=/$TESTPOOL/foo bs=1M count=1024

# Verify that the pool is online and exists in the bucket
typeset exp_allocated=$((1 << 30))
verify_active_object_store_pool $TESTPOOL $guid $exp_allocated

# Set object_deletion_batch_size to 10 (default is 1000), so that pool
# deletion in object store proceeds slowly and we are able to test resume
# operation.
add_or_update_tunable object_deletion_batch_size 10

# Destroy the pool
log_must zpool destroy $TESTPOOL

# Verify that the pool is in DESTROYING state
check_destroyed_pool_status $guid "state" "DESTROYING"

# Stop the agent
stop_zfs_object_agent

# Set object_deletion_batch_size back to 1000, so that the deletion progresses
# fast
add_or_update_tunable object_deletion_batch_size 1000

# Start the agent
start_zfs_object_agent

# Verify that the pool is destroyed
verify_destroyed_object_store_pool $TESTPOOL $guid

# Clear destroyed pool and verify that the pool no longer shows in
# zpool_destroy.cache
zpool status --clear-destroyed
log_mustnot cat /etc/zfs/zpool_destroy.cache | grep $guid

log_pass "Restarting the zfs object agent successfully resumed" \
	" zpool destroy operation"
