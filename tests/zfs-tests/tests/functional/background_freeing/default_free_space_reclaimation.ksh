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
#	The freed objects are stored in a pending frees log.
#	By default, when this free space represents 10% of the pool
#	these freed blocks are deleted and free space is reclaimed.
#
# STRATEGY:
#	1. Create a pool
#	2. Set zfs recordsize as 8K and compression as lz4 for the pool
#	3. Write out few(3) 100mb testdata files
#	4. Wait some time and do a sync so that all data is written
#	   back to s3
#	5. Verify from s3 that the PendingFreesLog contains the objects
#	6. Delete all testdata files
#	7. Verify from the zfs-object-agent log file that reclaimation
#	   happened and it was able to free the blocks

if ! use_object_store; then
	log_unsupported "Not supported for block based runs."
fi

verify_runnable "global"

function verify_reclaimation
{
	# Max 10 retries
	typeset max_retries="${1:-10}"
	typeset retry_count=0
	typeset wait_fixed=3

	typeset pattern="reclaim: using ReclaimLogId"

	while [ $retry_count -lt $max_retries ]; do
		match_count=$(grep -c "$pattern" $ZOA_DEBUG_LOG)
		log_note "Found $match_count instances of '$pattern' from $ZOA_DEBUG_LOG"
		[ $match_count -ge 1 ] && return 0
		retry_count=$((retry_count+1))
		sleep $wait_fixed
	done
	return 1
}

ZOA_DEBUG_LOG=$(get_zoa_debug_log)
log_assert "Verify that background freeing occurs when freed space represents" \
	" 10%(default) of the poolsize"

log_must zfs set recordsize=8k $TESTPOOL
log_must zfs set compression=lz4 $TESTPOOL

for i in {1..3}; do
	# Write 100Mb of data
	log_must sudo dd if=/dev/urandom of=/$TESTPOOL/data.$i \
		bs=8K count=12800
done

log_must zpool sync

typeset guid=$(get_object_store_pool_guid $TESTPOOL)

#
# Ensure guid is valid
#
log_must test "$guid" != "00000000000000000000"

typeset -i num_pending_frees_splits=0

if [ $ZTS_OBJECT_STORE == "s3" ]; then
	num_pending_frees_splits=$(aws --endpoint-url $ZTS_OBJECT_ENDPOINT \
		s3 ls $ZTS_BUCKET_NAME/zfs/$guid/PendingFreesLog/ | wc -l)
elif [ $ZTS_OBJECT_STORE == "blob" ]; then
	# This list all objects, to get the split count
	# extract the 4th column separated by '/'
	# zfs/08878726368137311436/PendingFreesLog/00000/00000000000000000000/...
	num_pending_frees_splits=$(az storage blob list -c $ZTS_BUCKET_NAME \
		--account-name $AZURE_ACCOUNT --account-key $AZURE_KEY \
		--output table \
		--prefix zfs/$guid/PendingFrees 2>/dev/null \
		| awk -F '/' '/zfs/ {print $4}' \
		| sort -u | wc -l)
fi

log_note "Total no of pending frees log split $num_pending_frees_splits"
#
# The pending frees log can hold approximately 10 million objects
# When the pending free objects in a log exceeds 10 million,
# then it will split the log into two
# The maximum is up to 2^16 splits.
#
# There should be atleast 1 split for a small dataset
#
log_must test $num_pending_frees_splits -gt 0

#
# For each dataset find out how many objects are currently there.
# A recursive call to list the parent object (PendingFreesLog)
# can help in summarizing the child count
#
typeset -i num_pending_frees_objects=0

if [ $ZTS_OBJECT_STORE == "s3" ]; then
	num_pending_frees_objects=$(aws --endpoint-url $ZTS_OBJECT_ENDPOINT \
		s3 ls $ZTS_BUCKET_NAME/zfs/$guid/PendingFreesLog/ --recursive | wc -l)
elif [ $ZTS_OBJECT_STORE == "blob" ]; then
	num_pending_frees_objects=$(az storage blob list -c $ZTS_BUCKET_NAME \
		--account-name $AZURE_ACCOUNT --account-key $AZURE_KEY \
		--output table \
		--prefix zfs/$guid/PendingFrees 2>/dev/null | awk '/zfs/' | wc -l)
fi

log_note "Total no of pending frees objects $num_pending_frees_objects"
log_must test $num_pending_frees_objects -gt 0

# Remove the testdata
for i in {1..3}; do
	log_must rm -vf /$TESTPOOL/data.$i
done

log_must verify_reclaimation
log_pass "Background freeing completed successfully for default settings"
