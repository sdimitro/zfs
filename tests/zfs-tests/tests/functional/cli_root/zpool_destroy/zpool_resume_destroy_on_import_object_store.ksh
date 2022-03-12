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
#	Verify that importing pool successfully resumes zpool destroy operation
#
# STRATEGY:
#	1. Create an object store based pool
#	2. Set object_deletion_batch_size to 10 (default is 1000), so that pool
#	   deletion in object store proceeds slowly and we are able to test resume
#	   operation
#	3. Destroy the pool
#	4. Verify that the pool is in DESTROYING state
#	5. Stop the agent
#	6. Delete the destroy pool cache file
#	7. Set object_deletion_batch_size back to 1000, so that the deletion
#		progresses fast
#	8. Start the agent
#	9. The destroy operation won't resume automatically because there is no
#		cache file. Resume destroy by zpool import operation
#	10. Verify the pool is destroyed successfully
#	11. Verify the object store space is freed up
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

log_assert "Verify that importing pool successfully resumes zpool destroy " \
	"operation"

log_onexit cleanup

create_pool -p $TESTPOOL
typeset guid=$(get_object_store_pool_guid $TESTPOOL)

log_must zfs set compression=off $TESTPOOL

# populate the zpool with 1 GB data
sudo dd if=/dev/zero of=/$TESTPOOL/foo bs=1M count=1024

# Verify that the pool is online and exists in the bucket
typeset exp_allocated=$((1 << 30))
verify_active_object_store_pool $TESTPOOL $guid $exp_allocated

# Set object_deletion_batch_size to 10 (default is 1000), so that pool
# deletion in object store proceeds slowly and we are able to test resume
# operation.
add_or_update_tunable object_deletion_batch_size 10

# Stop and start the zfs object agent to be able to pick up new
# object deletion batch size tunable.
stop_zfs_object_agent
start_zfs_object_agent

# Destroy the pool
log_must zpool destroy $TESTPOOL

# Verify that the pool is in DESTROYING state, keeping the retry count
# and sleep duration to be zero.

# For CI runs based on minio the destroy happens very
# quickly and thus we may not want to spend even a second
# waiting.
# The result of this is that import pool operation with resume(-r) option
# fails if there are no objects to destroy.
check_destroyed_pool_status $guid "state" "DESTROYING" 0 0

# Stop the agent
stop_zfs_object_agent

# Remove the destroy pool cache so that zpool destroy doesn't resume
# automatically
log_must rm /etc/zfs/zpool_destroy.cache

# Set object_deletion_batch_size back to 1000, so that the deletion progresses
# fast
add_or_update_tunable object_deletion_batch_size 1000

# Start the agent
start_zfs_object_agent

# Ensure that the pool guid has no leading zeros
# The function get_object_store_pool_guid
# pads the string with 0's to make it a length of 20
pool_guid=$(bc <<< $guid)

# Resume destroy by import operation
log_must import_pool -p $pool_guid -e "-r"

# Verify that the pool is destroyed
verify_destroyed_object_store_pool $TESTPOOL $guid

# Clear destroyed pool and verify that the pool no longer shows in
# zpool_destroy.cache
zpool status --clear-destroyed
log_mustnot cat /etc/zfs/zpool_destroy.cache | grep $guid

log_pass "Importing pool successfully resumed zpool destroy operation"
