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
#	Verify that the zfs-object-agent can dump in memory tracing
#	to a logfile when the SIGUSR1 signal is sent to the process
#
# STRATEGY:
#	1. Create a object store based test pool & write some data to it
#	2. Find the PID of the zfs-object-agent
#	3. Send SIGUSR1(kill -10) to the zoa process
#	4. Verify from /var/log/zoa/debug.log that the file
#	   was created
#	5. Verify that the file is not empty
#	6. Verify the file name pattern
#	7. Verify that the zfs object agent was not restarted

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Test not applicable for block based run"
fi

function cleanup
{
	poolexists $TESTPOOL && destroy_pool $TESTPOOL
}

log_onexit cleanup

# Truncate the debug log as it may contain
# multiple dump information
typeset zoa_debug_log=$(get_zoa_debug_log)
log_must truncate -s 0 $zoa_debug_log

log_must create_pool -p $TESTPOOL

# Write some data
log_must dd if=/dev/urandom of=/$TESTPOOL/datafile1 bs=1M count=10
log_must zpool sync

typeset zoa_pid=$(pgrep -f "/sbin/zfs_object_agent")
log_note "Found zfs-object-agent running with PID: $zoa_pid"

# Send the SIGUSR1 signal to the pid
log_must kill -SIGUSR1 $zoa_pid

# Try to retrieve the trace log message from the debug log
# file with some polling
typeset max_retries=5
typeset retry_interval=1
typeset retry_count=0

while [ $retry_count -lt $max_retries ]; do
	trace_log=$(awk '/dumping info to/ {print $6}' $zoa_debug_log)
	[[ -n "$trace_log" ]] && break
	sleep $retry_interval
	retry_count=$((retry_count+1))
done

# Verify that the trace file was found in debug.log
log_must test -n "$trace_log"

# Verify trace file was not empty
log_must test -s "$trace_log"

# Verify that the trace file was indeed for the current PID
log_must eval "[[ $trace_log =~ SIGUSR1_pid${zoa_pid} ]]"

# Verify that the object agent is still running with same PID
# and has not restarted after the SIGUSR1 signal was sent.
log_must ps -p $zoa_pid

log_pass "Successfully verified that the zfs-object-agent " \
	"can dump memory info into the trace file on receiving SIGUSR1"
