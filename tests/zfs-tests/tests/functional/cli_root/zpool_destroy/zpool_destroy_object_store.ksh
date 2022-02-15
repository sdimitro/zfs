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
#	'zpool destroy <pool>' can successfully destroy the specified object
#	store pool and free the object store space.
#
# STRATEGY:
#	1. Create an object store based pool
#	2. Destroy the pool
#	3. Verify the pool is destroyed successfully
#	4. Verify the object store space is freed up
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

log_assert "'zpool destroy <pool>' can destroy an object store pool"

log_onexit cleanup

create_pool -p $TESTPOOL
typeset guid=$(get_object_store_pool_guid $TESTPOOL)

# populate the zpool with 1 GB data
sudo dd if=/dev/zero of=/$TESTPOOL/foo bs=1M count=1024

# Verify that the pool is online and exists in the bucket
typeset exp_allocated=$((1 << 30))
verify_active_object_store_pool $TESTPOOL $guid $exp_allocated

# Destroy the pool
log_must zpool destroy $TESTPOOL

# Verify that the pool is destroyed
verify_destroyed_object_store_pool $TESTPOOL $guid

# Clear destroyed pool and verify that the pool no longer shows in
# zpool_destroy.cache
zpool status --clear-destroyed
log_mustnot cat /etc/zfs/zpool_destroy.cache | grep $guid

log_pass "'zpool destroy <pool>' for object store pool executes successfully"
