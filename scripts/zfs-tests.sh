#!/bin/sh
# shellcheck disable=SC2154,SC2155
#
# CDDL HEADER START
#
# The contents of this file are subject to the terms of the
# Common Development and Distribution License, Version 1.0 only
# (the "License").  You may not use this file except in compliance
# with the License.
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
# Copyright 2020 OmniOS Community Edition (OmniOSce) Association.
#

BASE_DIR=$(dirname "$0")
SCRIPT_COMMON=common.sh
if [ -f "${BASE_DIR}/${SCRIPT_COMMON}" ]; then
	. "${BASE_DIR}/${SCRIPT_COMMON}"
else
	echo "Missing helper script ${SCRIPT_COMMON}" && exit 1
fi

PROG=zfs-tests.sh
VERBOSE="no"
QUIET=""
CLEANUP="yes"
CLEANUPALL="no"
KMSG=""
LOOPBACK="yes"
STACK_TRACER="no"
FILESIZE="4G"
DEFAULT_RUNFILES="common.run,$(uname | tr '[:upper:]' '[:lower:]').run"
RUNFILES=${RUNFILES:-$DEFAULT_RUNFILES}
FILEDIR=${FILEDIR:-/var/tmp}
DISKS=${DISKS:-""}
SINGLETEST=""
SINGLETESTUSER="root"
TAGS=""
ITERATIONS=1
ZFS_DBGMSG="$STF_SUITE/callbacks/zfs_dbgmsg.ksh"
ZFS_DMESG="$STF_SUITE/callbacks/zfs_dmesg.ksh"
UNAME=$(uname)
RERUN=""
KMEMLEAK=""
# Export zfs_object_agent variables to be used in tests
#
export ZOA_LOG="/var/tmp/zoa.log"
export ZOA_OUTPUT="/var/tmp/zoa.stdout"
export ZOA_CONF="/etc/zfs/zoa.conf"
export ZOA_CONFIG="/etc/zfs/zoa_config.toml"
export HAS_ZOA_SERVICE="$(systemctl list-unit-files 2>/dev/null | \
    awk '/^zfs-object-agent/ {found=1}
    END{if(found) print "true"; else print "false"}')"

ZOA_TUNABLE_LIST="die_mtbf_secs die_file"
ZOA_DIE_MTBF_SECS_DEFAULT_VALUE="150"
#
# Create a marker file for finding crash dumps that
# was created during the test run.
# The find command can take this with argument -newer
# to print crashes whose timestamp was greater than
# the marker file
#
MARKER_FILE=$(mktemp)

# Override some defaults if on FreeBSD
if [ "$UNAME" = "FreeBSD" ] ; then
	TESTFAIL_CALLBACKS=${TESTFAIL_CALLBACKS:-"$ZFS_DMESG"}
	LOSETUP=/sbin/mdconfig
	DMSETUP=/sbin/gpart
else
	ZFS_MMP="$STF_SUITE/callbacks/zfs_mmp.ksh"
	TESTFAIL_CALLBACKS=${TESTFAIL_CALLBACKS:-"$ZFS_DBGMSG:$ZFS_DMESG:$ZFS_MMP"}
	LOSETUP=${LOSETUP:-/sbin/losetup}
	DMSETUP=${DMSETUP:-/sbin/dmsetup}
fi

#
# Log an informational message when additional verbosity is enabled.
#
msg() {
	if [ "$VERBOSE" = "yes" ]; then
		echo "$@"
	fi
}

#
# Log a failure message, cleanup, and return an error.
#
fail() {
	echo "$PROG: $1" >&2
	cleanup
	exit 1
}

cleanup_freebsd_loopback() {
	for TEST_LOOPBACK in ${LOOPBACKS}; do
		if [ -c "/dev/${TEST_LOOPBACK}" ]; then
			sudo "${LOSETUP}" -d -u "${TEST_LOOPBACK}" ||
			    echo "Failed to destroy: ${TEST_LOOPBACK}"
		fi
	done
}

cleanup_linux_loopback() {
	for TEST_LOOPBACK in ${LOOPBACKS}; do
		LOOP_DEV="${TEST_LOOPBACK##*/}"
		DM_DEV=$(sudo "${DMSETUP}" ls 2>/dev/null | \
		    awk -v l="${LOOP_DEV}" '$0 ~ l {print $1}')

		if [ -n "$DM_DEV" ]; then
			sudo "${DMSETUP}" remove "${DM_DEV}" ||
			    echo "Failed to remove: ${DM_DEV}"
		fi

		if [ -n "${TEST_LOOPBACK}" ]; then
			sudo "${LOSETUP}" -d "${TEST_LOOPBACK}" ||
			    echo "Failed to remove: ${TEST_LOOPBACK}"
		fi
	done
}

#
# Attempt to remove loopback devices and files which where created earlier
# by this script to run the test framework.  The '-k' option may be passed
# to the script to suppress cleanup for debugging purposes.
#
cleanup() {
	if [ "$CLEANUP" = "no" ]; then
		return 0
	fi

	if [ "$LOOPBACK" = "yes" ]; then
		if [ "$UNAME" = "FreeBSD" ] ; then
			cleanup_freebsd_loopback
		else
			cleanup_linux_loopback
		fi
	fi

	# shellcheck disable=SC2086
	rm -f ${FILES} >/dev/null 2>&1


	# Unset ZETTACACHE_DEVICES and invalidate zcache devices
	if [ -n "$ZTS_OBJECT_STORE" ]; then
		sudo systemctl stop zfs-object-agent
		for cache_dev in ${ZETTACACHE_DEVICES}; do
			invalidate_zcache_dev "$cache_dev"
		done

		sudo -E sed -i 's/ZETTACACHE_DEVICES=.*/ZETTACACHE_DEVICES=/g' \
		    "$ZOA_CONF"
		sudo systemctl start zfs-object-agent
	fi

	# Find all the crash files that were created after the start
	# of the test
	crash_files=$(find /var/crash -name "core.*" -newer "$MARKER_FILE")

	# If the list is not empty we try to find the binary name that
	# caused the crash & copy it to the $RESULTS_DIR
	if [ -n "$crash_files" ]; then
		# Try to infer from the output of file command
		crash_binaries=$(echo "$crash_files" | xargs sudo file | \
			sed -n 's/.*execfn: .\(.*\).,.*/\1/p' | sort | uniq
		)

		# Try figure out from the crash file name
		# The crash file name is of following format
		# core.<name>.<pid>.<timestamp>
		if [ -z "$crash_binaries" ]; then
			crash_binaries=$(echo "$crash_files" | cut -d '.' -f2 | \
				sort | uniq | xargs which
			)
		fi

		# Get the list of shared dependencies from the binaries
		dependencies=$(echo "$crash_binaries" | xargs ldd | \
			awk '/=>/ {print $3 }'
		)
		# Add the binaries to the list of dependencies
		# as those were not included in the previous step
		dependencies="$crash_binaries $dependencies"
		# Copy the shared files and its dependencies
		for dependency in $dependencies; do
			[ -e "$dependency" ] || continue
			if [ "$(basename "$dependency")" = "zdb" ]; then
				# Check & move the zdb test logs to a different
				# directory. Else it'll raise conflict copying
				# the crash binary
				[ -d "$RESULTS_DIR/zdb" ] && \
					mv "$RESULTS_DIR/zdb" "$RESULTS_DIR/zdb-test-log"
			fi
			cp "$dependency" "$RESULTS_DIR"

			[ -d "$RESULTS_DIR/.build-id" ] || \
				mkdir -p  "$RESULTS_DIR/.build-id"

			uuid=$(readelf -n "$dependency" | awk '/Build ID/ {print $3}')
			[ -n "$uuid" ] || continue

			prefix="$(echo "$uuid" | cut -c1-2)"
			debug_file="$(echo "$uuid" | cut -c3-40).debug"

			[ -f "/usr/lib/debug/.build-id/$prefix/$debug_file" ] || continue

			mkdir -p "$RESULTS_DIR/.build-id/$prefix"
			cp "/usr/lib/debug/.build-id/$prefix/$debug_file" \
				"$RESULTS_DIR/.build-id/$prefix"
		done

		# Copy the crash files to $RESULTS_DIR/crash
		mkdir -p "$RESULTS_DIR/crash"
		echo "$crash_files" | xargs -I{} sudo cp {} "$RESULTS_DIR/crash"
		# Change the ownership of core files from root to current user
		sudo chown "$(id -un):$(id -gn)" -R "$RESULTS_DIR/crash"

		# Create a convenience script to launch a gdb debugging session
		# with options to load the debug information just gathered
		cat >"$RESULTS_DIR/run-gdb.sh" <<-EOF
			#!/usr/bin/env bash
			crash_dir="\$(realpath \$(dirname \$0))/crash"
			debug_dir="\$(realpath \$(dirname \$0))"
			core_file="\$1"

			if [ ! -f "\$core_file" ]; then
			    core_file="\$crash_dir/\$(basename \$core_file)"
			    [ ! -f "\$core_file" ] && echo "Not a valid core file" && exit 1
			fi

			core_prog=\$(file \$core_file | \\
			    sed -n 's/.*execfn: .\\(.*\\).,.*/\\1/p' | \\
			    xargs basename
			)
			# Fallback to find core name from the prog itself
			if [ -z "\$core_prog" ]; then
			    core_prog=\$(basename \$core_file | cut -d '.' -f2)
			fi

			gdb -iex "set print thread-events off" \\
			    -iex "set sysroot /dev/null" \\
			    -iex "set debug-file-directory \$debug_dir" \\
			    -iex "set solib-search-path \$debug_dir" \\
			    -iex "file \$debug_dir/\$core_prog" \\
			    -iex "core-file \$core_file"

		EOF
		chmod +x "$RESULTS_DIR/run-gdb.sh"
	fi

	# Finally remove the marker file
	rm -f "$MARKER_FILE"

	# From this point onwards, the script will run with an empty $PATH
	if [ "$STF_PATH_REMOVE" = "yes" ] && [ -d "$STF_PATH" ]; then
		rm -Rf "$STF_PATH"
	fi
}
trap cleanup EXIT

#
# Attempt to remove all testpools (testpool.XXX), unopened dm devices,
# loopback devices, and files.  This is a useful way to cleanup a previous
# test run failure which has left the system in an unknown state.  This can
# be dangerous and should only be used in a dedicated test environment.
#
cleanup_all() {
	TEST_POOLS=$(ASAN_OPTIONS=detect_leaks=false "$ZPOOL" list -Ho name | grep testpool)
	if [ "$UNAME" = "FreeBSD" ] ; then
		TEST_LOOPBACKS=$(sudo "${LOSETUP}" -l)
	else
		TEST_LOOPBACKS=$("${LOSETUP}" -a | awk -F: '/file-vdev/ {print $1}')
	fi
	TEST_FILES=$(ls "${FILEDIR}"/file-vdev* /var/tmp/file-vdev* 2>/dev/null)

	msg
	msg "--- Cleanup ---"
	# shellcheck disable=2116,2086
	msg "Removing pool(s):     $(echo ${TEST_POOLS})"
	for TEST_POOL in $TEST_POOLS; do
		sudo env ASAN_OPTIONS=detect_leaks=false "$ZPOOL" destroy "${TEST_POOL}"
	done

	if [ "$UNAME" != "FreeBSD" ] ; then
		msg "Removing all dm(s):   $(sudo "${DMSETUP}" ls |
		    grep loop | tr '\n' ' ')"
		sudo "${DMSETUP}" remove_all
	fi

	# shellcheck disable=2116,2086
	msg "Removing loopback(s): $(echo ${TEST_LOOPBACKS})"
	for TEST_LOOPBACK in $TEST_LOOPBACKS; do
		if [ "$UNAME" = "FreeBSD" ] ; then
			sudo "${LOSETUP}" -d -u "${TEST_LOOPBACK}"
		else
			sudo "${LOSETUP}" -d "${TEST_LOOPBACK}"
		fi
	done

	# shellcheck disable=2116,2086
	msg "Removing files(s):    $(echo ${TEST_FILES})"
	# shellcheck disable=2086
	sudo rm -f ${TEST_FILES}
}

#
# Takes a name as the only arguments and looks for the following variations
# on that name.  If one is found it is returned.
#
# $RUNFILE_DIR/<name>
# $RUNFILE_DIR/<name>.run
# <name>
# <name>.run
#
find_runfile() {
	NAME=$1

	if [ -f "$RUNFILE_DIR/$NAME" ]; then
		echo "$RUNFILE_DIR/$NAME"
	elif [ -f "$RUNFILE_DIR/$NAME.run" ]; then
		echo "$RUNFILE_DIR/$NAME.run"
	elif [ -f "$NAME" ]; then
		echo "$NAME"
	elif [ -f "$NAME.run" ]; then
		echo "$NAME.run"
	else
		return 1
	fi
}

#
# Symlink file if it appears under any of the given paths.
#
create_links() {
	dir_list="$1"
	file_list="$2"

	[ -n "$STF_PATH" ] || fail "STF_PATH wasn't correctly set"

	for i in $file_list; do
		for j in $dir_list; do
			[ ! -e "$STF_PATH/$i" ] || continue

			if [ ! -d "$j/$i" ] && [ -e "$j/$i" ]; then
				ln -sf "$j/$i" "$STF_PATH/$i" || \
				    fail "Couldn't link $i"
				break
			fi
		done

		[ ! -e "$STF_PATH/$i" ] && \
		    STF_MISSING_BIN="$STF_MISSING_BIN $i"
	done
	STF_MISSING_BIN=${STF_MISSING_BIN# }
}

#
# Constrain the path to limit the available binaries to a known set.
# When running in-tree a top level ./bin/ directory is created for
# convenience, otherwise a temporary directory is used.
#
constrain_path() {
	. "$STF_SUITE/include/commands.cfg"

	# On FreeBSD, base system zfs utils are in /sbin and OpenZFS utils
	# install to /usr/local/sbin. To avoid testing the wrong utils we
	# need /usr/local to come before / in the path search order.
	SYSTEM_DIRS="/usr/local/bin /usr/local/sbin"
	SYSTEM_DIRS="$SYSTEM_DIRS /usr/bin /usr/sbin /bin /sbin $LIBEXEC_DIR"

	if [ "$INTREE" = "yes" ]; then
		# Constrained path set to ./zfs/bin/
		STF_PATH="$BIN_DIR"
		STF_PATH_REMOVE="no"
		STF_MISSING_BIN=""
		if [ ! -d "$STF_PATH" ]; then
			mkdir "$STF_PATH"
			chmod 755 "$STF_PATH" || fail "Couldn't chmod $STF_PATH"
		fi

		# Special case links for standard zfs utilities
		DIRS="$(find "$CMD_DIR" -type d \( ! -name .deps -a \
		    ! -name .libs \) -print | tr '\n' ' ')"
		create_links "$DIRS" "$ZFS_FILES"

		# Special case links for zfs test suite utilities
		DIRS="$(find "$STF_SUITE" -type d \( ! -name .deps -a \
		    ! -name .libs \) -print | tr '\n' ' ')"
		create_links "$DIRS" "$ZFSTEST_FILES"
	else
		# Constrained path set to /var/tmp/constrained_path.*
		SYSTEMDIR=${SYSTEMDIR:-/var/tmp/constrained_path.XXXXXX}
		STF_PATH=$(mktemp -d "$SYSTEMDIR")
		STF_PATH_REMOVE="yes"
		STF_MISSING_BIN=""

		chmod 755 "$STF_PATH" || fail "Couldn't chmod $STF_PATH"

		# Special case links for standard zfs utilities
		create_links "$SYSTEM_DIRS" "$ZFS_FILES"

		# Special case links for zfs test suite utilities
		create_links "$STF_SUITE/bin" "$ZFSTEST_FILES"
	fi

	# Standard system utilities
	SYSTEM_FILES="$SYSTEM_FILES_COMMON"
	if [ "$UNAME" = "FreeBSD" ] ; then
		SYSTEM_FILES="$SYSTEM_FILES $SYSTEM_FILES_FREEBSD"
	else
		SYSTEM_FILES="$SYSTEM_FILES $SYSTEM_FILES_LINUX"
	fi
	create_links "$SYSTEM_DIRS" "$SYSTEM_FILES"

	# Exceptions
	if [ "$UNAME" = "Linux" ] ; then
		ln -fs /sbin/fsck.ext4 "$STF_PATH/fsck"
		ln -fs /sbin/mkfs.ext4 "$STF_PATH/newfs"
		ln -fs "$STF_PATH/gzip" "$STF_PATH/compress"
		ln -fs "$STF_PATH/gunzip" "$STF_PATH/uncompress"
	elif [ "$UNAME" = "FreeBSD" ] ; then
		ln -fs /usr/local/bin/ksh93 "$STF_PATH/ksh"
	fi
}

#
# Output a useful usage message.
#
usage() {
cat << EOF
USAGE:
$0 [-hvqxkfS] [-s SIZE] [-r RUNFILES] [-t PATH] [-u USER]

DESCRIPTION:
	ZFS Test Suite launch script

OPTIONS:
	-h          Show this message
	-v          Verbose zfs-tests.sh output
	-q          Quiet test-runner output
	-x          Remove all testpools, dm, lo, and files (unsafe)
	-k          Disable cleanup after test failure
	-K          Log test names to /dev/kmsg
	-f          Use files only, disables block device tests
	-S          Enable stack tracer (negative performance impact)
	-c          Only create and populate constrained path
	-R          Automatically rerun failing tests
	-m          Enable kmemleak reporting (Linux only)
	-n NFSFILE  Use the nfsfile to determine the NFS configuration
	-I NUM      Number of iterations
	-d DIR      Use world-writable DIR for files and loopback devices
	-s SIZE     Use vdevs of SIZE (default: 4G)
	-r RUNFILES Run tests in RUNFILES (default: ${DEFAULT_RUNFILES})
	-t PATH     Run single test at PATH relative to test suite
	-T TAGS     Comma separated list of tags (default: 'functional')
	-u USER     Run single test as USER (default: root)

EXAMPLES:
# Run the default ($(echo "${DEFAULT_RUNFILES}" | sed 's/\.run//')) suite of tests and output the configuration used.
$0 -v

# Run a smaller suite of tests designed to run more quickly.
$0 -r linux-fast

# Run a single test
$0 -t tests/functional/cli_root/zfs_bookmark/zfs_bookmark_cliargs.ksh

# Cleanup a previous run of the test suite prior to testing, run the
# default ($(echo "${DEFAULT_RUNFILES}" | sed 's/\.run//')) suite of tests and perform no cleanup on exit.
$0 -x

EOF
}

# Take a Zettacache device as either an absolute or relative path and
# return the /dev/disk/by-id name for the cache partition.
get_cache_part() {
	devname="$1"
	cache_part=""

	[ -z "$devname" ] && fail "Missing argument"

	if echo "$devname" | grep -q "^/dev/disk/by-id/"; then
		cache_part="${devname}-part2"
	elif echo "$devname" | grep -q "nvme"; then
		devname="$(basename "$devname")"
		cache_part="/dev/${devname}p2"
	else
		devname="$(basename "$devname")"
		cache_part="/dev/${devname}2"
	fi

	udevadm settle -E "$cache_part"
	echo "$cache_part"
}

invalidate_zcache_dev() {
	cache_dev="$1"
	cache_part="$(get_cache_part "$cache_dev")"
	sudo dd if=/dev/zero of="${cache_part}" bs=1M count=1 >/dev/null 2>&1
}

configure_zettacache() {
	cache_parts=""
	for cache_dev in ${ZETTACACHE_DEVICES}; do
		#
		# Dedicate 8G at the start of the zettacache disk for a slog.
		# Devices are specified by /dev/ or /dev/disk/by-id/ names.
		#
		if echo "$cache_dev" | grep -q "^/dev/disk/by-id/"; then
			printf "size=16777216, bootable\n," | \
			    sudo sfdisk -q -X gpt --wipe always "$cache_dev"
		else
			printf "size=16777216, bootable\n," | \
			    sudo sfdisk -q -X gpt --wipe always \
			    "/dev/$(basename "$cache_dev")"
		fi

		invalidate_zcache_dev "$cache_dev"
		if [ -z "$cache_parts" ]; then
			cache_parts="$(get_cache_part "$cache_dev")"
		else
			cache_parts="${cache_parts},$(get_cache_part "$cache_dev")"
		fi
	done
	sudo -E sed -i '/ZETTACACHE_DEVICES=.*/d' "$ZOA_CONF"
	sudo sh -c "echo ZETTACACHE_DEVICES=$cache_parts >>$ZOA_CONF"
}

# Add a tunable with name and value in the
# /etc/zfs/zoa_config.toml
add_tunable() {
    name="$1"
    value="$2"
    echo "$name=$value" | sudo tee -a "$ZOA_CONFIG" > /dev/null
}

# Returns if a tunable is already configured
# in the zoa configuration file
is_tunable_configured() {
    grep "$1" "$ZOA_CONFIG" 1>/dev/null 2>&1
    return $?
}

# Adds or updates a tunable into the /etc/zfs/zoa_config.toml
add_or_update_tunable() {
    name="$1"
    value="$2"

    if is_tunable_configured "$name"; then
        # sed -E enables extended regexp

        # Anything that has 0 or more whitespace
        # followed by keyword identified by $name
        # followed by 0 or more white spaces
        # followed by a =
        # followed by 0 or more white spaces
        # followed by group that captures anything
        sudo -E \
            sed -E -i "s/\s*${name}\s*=\s*(.*)/${name}=${value}/" "$ZOA_CONFIG"
    else
        add_tunable "$name" "$value"
    fi
}

# Returns if the tunable is in allowed list
is_tunable_allowed() {
    tunable_name="$1"
    for tunable in $ZOA_TUNABLE_LIST; do
        if [ "$tunable" = "$tunable_name" ]; then
            return 0
        fi
    done
    return 1
}

# Checks and sets the ZOA tunables in
# the /etc/zfs/zoa_config.toml
check_and_set_zoa_tunables() {
    # A tunable can be defined using the environment
    # variable in following format
    # ZTS_ZOA_TUNABLE_<TUNABLE_NAME>=<TUNABLE_VALUE>

    # As a convention any environment variable prefixed
    # with ZTS_ZOA_TUNABLE_ would be considered as a
    # valid tunable and shall be added to the /etc/zfs/zoa_config.toml
    # only if the tunable is allowed

    # Loop through all environment variables that begins
    # with ZTS_ZOA_TUNABLE_ in a sorted form
    for zoa_tunable in $(env | grep "ZTS_ZOA_TUNABLE_" | sort | xargs); do
        # Chop the prefix
        tunable=${zoa_tunable#ZTS_ZOA_TUNABLE_}

        # Convert the tunable to its lowercase format
        # A tunable name is the first field separated
        # by a "="
        tunable_name=$(echo "$tunable" | cut -d "=" -f1 |\
            tr "[:upper:]" "[:lower:]")

        # Tunable value is the second field separated
        # by a "="
        tunable_value=$(echo "$tunable" | cut -d "=" -f2)

        if is_tunable_allowed "$tunable_name"; then
            add_or_update_tunable "$tunable_name" "$tunable_value"
        else
            msg "Skipping zoa tunable $tunable_name as it is not" \
                "in the allowed list of tunables"
        fi
    done
    # Finally if the variable ZTS_ZOA_TUNABLE_DIE_MTBF_SECS is not
    # set in the environment variable then add a default
    # one to the config
    if ! is_tunable_configured "die_mtbf_secs"; then
        add_tunable "die_mtbf_secs" "$ZOA_DIE_MTBF_SECS_DEFAULT_VALUE"
    fi
}


# Checks if the S3 credentials are available
# for the connectivity test
are_s3_credentials_available() {
	[ -n "$AWS_ACCESS_KEY_ID" ] && [ -n "$AWS_SECRET_ACCESS_KEY" ] && \
		return 0 || return 1
}


# Tests the S3 connectivity using the s3 credentials
# or the instance profile.
# To test using instance profile pass "true"
# as the first positional argument
test_s3_connectivity() {
	# Flag to check if connectivity should be
	# tested using the instance profile
	# Defaults to false
	use_instance_profile="${1:-false}"

	# Build the common part
	zoa_cmd="/sbin/zfs_object_agent test_connectivity"
	zoa_cmd="$zoa_cmd --region $ZTS_REGION"
	zoa_cmd="$zoa_cmd --endpoint $ZTS_OBJECT_ENDPOINT"
	zoa_cmd="$zoa_cmd --bucket $ZTS_BUCKET_NAME"

	if [ "$use_instance_profile" = "true" ]; then
		zoa_cmd="$zoa_cmd --aws_instance_profile"
	elif [ "$use_instance_profile" = "false" ]; then
		zoa_cmd="$zoa_cmd --aws_access_key_id $AWS_ACCESS_KEY_ID"
		zoa_cmd="$zoa_cmd --aws_secret_access_key $AWS_SECRET_ACCESS_KEY"
	fi
	$zoa_cmd >/dev/null 2>&1 || fail "Unable to connect to S3"
}

# Configures and sets the S3 credentials to the disk
configure_and_set_s3_credentials() {
	# Check and comment out the AWS_ environment variables
	# from the /etc/environment file
	if grep -q "^AWS" /etc/environment 2>/dev/null; then
		sudo sed -i "s/^AWS/# AWS/g" /etc/environment
	fi
	# If aws cli is installed and is in path
	if command -v aws >/dev/null 2>&1; then
		aws configure set aws_access_key_id "$AWS_ACCESS_KEY_ID"
		aws configure set aws_secret_access_key "$AWS_SECRET_ACCESS_KEY"
		sudo mkdir -p /root/.aws && \
			sudo cp ~/.aws/credentials /root/.aws/credentials
	fi
}

while getopts 'hvqxkKfScRmn:d:s:r:?t:T:u:I:' OPTION; do
	case $OPTION in
	h)
		usage
		exit 1
		;;
	v)
		VERBOSE="yes"
		;;
	q)
		QUIET="yes"
		;;
	x)
		CLEANUPALL="yes"
		;;
	k)
		CLEANUP="no"
		;;
	K)
		KMSG="yes"
		;;
	f)
		LOOPBACK="no"
		;;
	S)
		STACK_TRACER="yes"
		;;
	c)
		constrain_path
		exit
		;;
	R)
		RERUN="yes"
		;;
	m)
		KMEMLEAK="yes"
		;;
	n)
		nfsfile=$OPTARG
		[ -f "$nfsfile" ] || fail "Cannot read file: $nfsfile"
		export NFS=1
		. "$nfsfile"
		;;
	d)
		FILEDIR="$OPTARG"
		;;
	I)
		ITERATIONS="$OPTARG"
		if [ "$ITERATIONS" -le 0 ]; then
			fail "Iterations must be greater than 0."
		fi
		;;
	s)
		FILESIZE="$OPTARG"
		;;
	r)
		RUNFILES="$OPTARG"
		;;
	t)
		if [ -n "$SINGLETEST" ]; then
			fail "-t can only be provided once."
		fi
		SINGLETEST="$OPTARG"
		;;
	T)
		TAGS="$OPTARG"
		;;
	u)
		SINGLETESTUSER="$OPTARG"
		;;
	?)
		usage
		exit
		;;
	*)
		;;
	esac
done

shift $((OPTIND-1))

FILES=${FILES:-"$FILEDIR/file-vdev0 $FILEDIR/file-vdev1 $FILEDIR/file-vdev2"}
LOOPBACKS=${LOOPBACKS:-""}

if [ -n "$SINGLETEST" ]; then
	if [ -n "$TAGS" ]; then
		fail "-t and -T are mutually exclusive."
	fi
	RUNFILE_DIR="/var/tmp"
	RUNFILES="zfs-tests.$$.run"
	[ -n "$QUIET" ] && SINGLEQUIET="True" || SINGLEQUIET="False"

	cat >"${RUNFILE_DIR}/${RUNFILES}" << EOF
[DEFAULT]
pre =
quiet = $SINGLEQUIET
pre_user = root
user = $SINGLETESTUSER
timeout = 600
post_user = root
post =
outputdir = /var/tmp/test_results
EOF
	SINGLETESTDIR="${SINGLETEST%/*}"

	SETUPDIR="$SINGLETESTDIR"
	[ "${SETUPDIR#/}" = "$SETUPDIR" ] && SETUPDIR="$STF_SUITE/$SINGLETESTDIR"
	[ -x "$SETUPDIR/setup.ksh"   ] && SETUPSCRIPT="setup"     || SETUPSCRIPT=
	[ -x "$SETUPDIR/cleanup.ksh" ] && CLEANUPSCRIPT="cleanup" || CLEANUPSCRIPT=

	SINGLETESTFILE="${SINGLETEST##*/}"
	cat >>"${RUNFILE_DIR}/${RUNFILES}" << EOF

[$SINGLETESTDIR]
tests = ['$SINGLETESTFILE']
pre = $SETUPSCRIPT
post = $CLEANUPSCRIPT
tags = ['functional']
EOF
fi

#
# Use default tag if none was specified
#
TAGS=${TAGS:='functional'}

#
# Attempt to locate the runfiles describing the test workload.
#
R=""
IFS=,
for RUNFILE in $RUNFILES; do
	if [ -n "$RUNFILE" ]; then
		SAVED_RUNFILE="$RUNFILE"
		RUNFILE=$(find_runfile "$RUNFILE") ||
			fail "Cannot find runfile: $SAVED_RUNFILE"
		R="$R,$RUNFILE"
	fi

	if [ ! -r "$RUNFILE" ]; then
		fail "Cannot read runfile: $RUNFILE"
	fi
done
unset IFS
RUNFILES=${R#,}

#
# This script should not be run as root.  Instead the test user, which may
# be a normal user account, needs to be configured such that it can
# run commands via sudo passwordlessly.
#
if [ "$(id -u)" = "0" ]; then
	fail "This script must not be run as root."
fi

if [ "$(sudo id -un)" != "root" ]; then
	fail "Passwordless sudo access required."
fi

#
# Constrain the available binaries to a known set.
#
constrain_path

#
# Check if ksh exists
#
if [ "$UNAME" = "FreeBSD" ]; then
	sudo ln -fs /usr/local/bin/ksh93 /bin/ksh
fi
[ -e "$STF_PATH/ksh" ] || fail "This test suite requires ksh."
[ -e "$STF_SUITE/include/default.cfg" ] || fail \
    "Missing $STF_SUITE/include/default.cfg file."

#
# Verify the ZFS module stack is loaded.
#
if [ "$STACK_TRACER" = "yes" ]; then
	sudo "${ZFS_SH}" -S >/dev/null 2>&1
else
	sudo "${ZFS_SH}" >/dev/null 2>&1
fi

#
# Attempt to cleanup all previous state for a new test run.
#
if [ "$CLEANUPALL" = "yes" ]; then
	cleanup_all
fi

#
# By default preserve any existing pools
#
if [ -z "${KEEP}" ]; then
	KEEP="$(ASAN_OPTIONS=detect_leaks=false "$ZPOOL" list -Ho name | tr -s '[:space:]' ' ')"
	if [ -z "${KEEP}" ]; then
		KEEP="rpool"
	fi
else
	KEEP="$(echo "$KEEP" | tr -s '[:space:]' ' ')"
fi

#
# NOTE: The following environment variables are undocumented
# and should be used for testing purposes only:
#
# __ZFS_POOL_EXCLUDE - don't iterate over the pools it lists
# __ZFS_POOL_RESTRICT - iterate only over the pools it lists
#
# See libzfs/libzfs_config.c for more information.
#
__ZFS_POOL_EXCLUDE="$KEEP"

. "$STF_SUITE/include/default.cfg"

#
# If ZTS_OBJECT_STORE is set, it implies that we are using object storage.
# Hence, no need to specify disks.
# If ZTS_OBJECT_STORE is not set and DISKS have not been provided, a basic file
# or loopback based devices must be created for the test suite to use.
#

if [ -n "$ZTS_OBJECT_STORE" ]; then
	# No need to specify disks if we're using object storage

	#
	# Ensure that all the required environment variables for object
	# storage are set. If any of them is unset, exit the script.
	#
	[ -n "$ZTS_OBJECT_ENDPOINT" ] || fail "ZTS_OBJECT_ENDPOINT is unset."
	[ -n "$ZTS_BUCKET_NAME" ] || fail "ZTS_BUCKET_NAME is unset."
	[ -n "$ZTS_REGION" ] || fail "ZTS_REGION is unset."
	[ -n "$ZTS_CREDS_PROFILE" ] || export ZTS_CREDS_PROFILE=default

	#
	# Set RUST_BACKTRACE environment variable to generate proper stack
	# traces for zfs_object_agent service crash.
	#
	export RUST_BACKTRACE=1

	# Use ZETTACACHE_DEVICE to be backward compatible
	ZETTACACHE_DEVICE=${ZETTACACHE_DEVICE:-""}
	if [ -n "$ZETTACACHE_DEVICE" ]; then
		export ZETTACACHE_DEVICES=$ZETTACACHE_DEVICE
		unset ZETTACACHE_DEVICE
	fi

	if [ -n "$ZETTACACHE_DEVICES" ]; then
		configure_zettacache
	else
		sudo -E sed -i 's/ZETTACACHE_DEVICES=.*/ZETTACACHE_DEVICES=/g' \
		    "$ZOA_CONF"
	fi

	# Enable zfs-object-agent to automatically
	# kill itself with the tunables set
	if [ -n "$ZTS_KILL_ZOA" ]; then
		check_and_set_zoa_tunables
	fi

	#
	# Start zfs_object_agent using the service if available, otherwise
	# start it manually.
	#
	if $HAS_ZOA_SERVICE; then
		sudo systemctl restart zfs-object-agent
	else
		sudo -E /sbin/zfs_object_agent -vv -t "$ZOA_CONFIG" \
		    --output-file="$ZOA_LOG" 2>&1 | \
		    sudo tee "$ZOA_OUTPUT" > /dev/null &
	fi

	#
	# Check connectivity to s3 and configure the system
	# to correctly run test either by using the S3 creds
	# or the instance profile role.
	#
	if are_s3_credentials_available; then
		test_s3_connectivity
		configure_and_set_s3_credentials
		msg "zfs-test for object storage configured" \
			"to run via S3 credentials"
	else
		# Test using instance profile
		test_s3_connectivity "true"
		# For running test using instance profile
		# we need to remove the underlying credentials
		# stored in the disk
		rm -f ~/.aws/credentials

		sudo rm -f /root/.aws/credentials
		msg "zfs-test for object storage configured" \
			"to run via the instance profile role"
	fi

elif [ -z "${DISKS}" ]; then
	#
	# If this is a performance run, prevent accidental use of
	# loopback devices.
	#
	[ "$TAGS" = "perf" ] && fail "Running perf tests without disks."

	#
	# Create sparse files for the test suite.  These may be used
	# directory or have loopback devices layered on them.
	#
	for TEST_FILE in ${FILES}; do
		[ -f "$TEST_FILE" ] && fail "Failed file exists: ${TEST_FILE}"
		truncate -s "${FILESIZE}" "${TEST_FILE}" ||
		    fail "Failed creating: ${TEST_FILE} ($?)"
	done

	#
	# If requested setup loopback devices backed by the sparse files.
	#
	if [ "$LOOPBACK" = "yes" ]; then
		test -x "$LOSETUP" || fail "$LOSETUP utility must be installed"

		for TEST_FILE in ${FILES}; do
			if [ "$UNAME" = "FreeBSD" ] ; then
				MDDEVICE=$(sudo "${LOSETUP}" -a -t vnode -f "${TEST_FILE}")
				if [ -z "$MDDEVICE" ] ; then
					fail "Failed: ${TEST_FILE} -> loopback"
				fi
				DISKS="$DISKS $MDDEVICE"
				LOOPBACKS="$LOOPBACKS $MDDEVICE"
			else
				TEST_LOOPBACK=$(sudo "${LOSETUP}" --show -f "${TEST_FILE}") ||
				    fail "Failed: ${TEST_FILE} -> ${TEST_LOOPBACK}"
				BASELOOPBACK="${TEST_LOOPBACK##*/}"
				DISKS="$DISKS $BASELOOPBACK"
				LOOPBACKS="$LOOPBACKS $TEST_LOOPBACK"
			fi
		done
		DISKS=${DISKS# }
		LOOPBACKS=${LOOPBACKS# }
	else
		DISKS="$FILES"
	fi
fi

#
# It may be desirable to test with fewer disks than the default when running
# the performance tests, but the functional tests require at least three.
#
if [ -z "$ZTS_OBJECT_STORE" ]; then
	NUM_DISKS=$(echo "${DISKS}" | awk '{print NF}')
	if [ "$TAGS" != "perf" ]; then
		[ "$NUM_DISKS" -lt 3 ] && fail "Not enough disks ($NUM_DISKS/3 minimum)"
	fi
fi

#
# Disable SELinux until the ZFS Test Suite has been updated accordingly.
#
if command -v setenforce >/dev/null; then
	sudo setenforce permissive >/dev/null 2>&1
fi

#
# Enable internal ZFS debug log and clear it.
#
if [ -e /sys/module/zfs/parameters/zfs_dbgmsg_enable ]; then
	sudo sh -c "echo 1 >/sys/module/zfs/parameters/zfs_dbgmsg_enable"
	sudo sh -c "echo 0 >/proc/spl/kstat/zfs/dbgmsg"
fi

msg
msg "--- Configuration ---"
msg "Runfiles:        $RUNFILES"
msg "STF_TOOLS:       $STF_TOOLS"
msg "STF_SUITE:       $STF_SUITE"
msg "STF_PATH:        $STF_PATH"
msg "FILEDIR:         $FILEDIR"
msg "FILES:           $FILES"
msg "LOOPBACKS:       $LOOPBACKS"
msg "DISKS:           $DISKS"
msg "NUM_DISKS:       $NUM_DISKS"
msg "FILESIZE:        $FILESIZE"
msg "ITERATIONS:      $ITERATIONS"
msg "TAGS:            $TAGS"
msg "STACK_TRACER:    $STACK_TRACER"
msg "Keep pool(s):    $KEEP"
msg "Missing util(s): $STF_MISSING_BIN"
msg "ZTS_OBJECT_STORE:      $ZTS_OBJECT_STORE"
msg "ZETTACACHE_DEVICES:     $ZETTACACHE_DEVICES"
msg "RUST_BACKTRACE:        $RUST_BACKTRACE"
msg "ZTS_KILL_ZOA:          $ZTS_KILL_ZOA"
msg ""

export STF_TOOLS
export STF_SUITE
export STF_PATH
export DISKS
export FILEDIR
export KEEP
export __ZFS_POOL_EXCLUDE
export TESTFAIL_CALLBACKS

mktemp_file() {
	if [ "$UNAME" = "FreeBSD" ]; then
		mktemp -u "${FILEDIR}/$1.XXXXXX"
	else
		mktemp -ut "$1.XXXXXX" -p "$FILEDIR"
	fi
}
mkdir -p "$FILEDIR" || :
RESULTS_FILE=$(mktemp_file zts-results)
REPORT_FILE=$(mktemp_file zts-report)

#
# Run all the tests as specified.
#
msg "${TEST_RUNNER}" \
    "${QUIET:+-q}" \
    "${KMEMLEAK:+-m}" \
    "${KMSG:+-K}" \
    "-c \"${RUNFILES}\"" \
    "-T \"${TAGS}\"" \
    "-i \"${STF_SUITE}\"" \
    "-I \"${ITERATIONS}\""
{ PATH=$STF_PATH \
    ${TEST_RUNNER} \
    ${QUIET:+-q} \
    ${KMEMLEAK:+-m} \
    ${KMSG:+-K} \
    -c "${RUNFILES}" \
    -T "${TAGS}" \
    -i "${STF_SUITE}" \
    -I "${ITERATIONS}" \
    2>&1; echo $? >"$REPORT_FILE"; } | tee "$RESULTS_FILE"
read -r RUNRESULT <"$REPORT_FILE"

#
# Analyze the results.
#
${ZTS_REPORT} ${RERUN:+--no-maybes} "$RESULTS_FILE" >"$REPORT_FILE"
RESULT=$?

if [ "$RESULT" -eq "2" ] && [ -n "$RERUN" ]; then
	MAYBES="$($ZTS_REPORT --list-maybes)"
	TEMP_RESULTS_FILE=$(mktemp_file zts-results-tmp)
	TEST_LIST=$(mktemp_file test-list)
	grep "^Test:.*\[FAIL\]" "$RESULTS_FILE" >"$TEMP_RESULTS_FILE"
	for test_name in $MAYBES; do
		grep "$test_name " "$TEMP_RESULTS_FILE" >>"$TEST_LIST"
	done
	{ PATH=$STF_PATH \
	    ${TEST_RUNNER} \
	        ${QUIET:+-q} \
	        ${KMEMLEAK:+-m} \
	    -c "${RUNFILES}" \
	    -T "${TAGS}" \
	    -i "${STF_SUITE}" \
	    -I "${ITERATIONS}" \
	    -l "${TEST_LIST}" \
	    2>&1; echo $? >"$REPORT_FILE"; } | tee "$RESULTS_FILE"
	read -r RUNRESULT <"$REPORT_FILE"
	#
	# Analyze the results.
	#
	${ZTS_REPORT} --no-maybes "$RESULTS_FILE" >"$REPORT_FILE"
	RESULT=$?
fi


cat "$REPORT_FILE"

RESULTS_DIR=$(awk '/^Log directory/ { print $3 }' "$RESULTS_FILE")
if [ -d "$RESULTS_DIR" ]; then
	cat "$RESULTS_FILE" "$REPORT_FILE" >"$RESULTS_DIR/results"
fi

rm -f "$RESULTS_FILE" "$REPORT_FILE" "$TEST_LIST" "$TEMP_RESULTS_FILE"

if [ -n "$SINGLETEST" ]; then
	rm -f "$RUNFILES" >/dev/null 2>&1
fi

[ "$RUNRESULT" -gt 3 ] && exit "$RUNRESULT" || exit "$RESULT"
