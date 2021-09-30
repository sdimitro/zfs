/*
 * CDDL HEADER START
 *
 * The contents of this file are subject to the terms of the
 * Common Development and Distribution License (the "License").
 * You may not use this file except in compliance with the License.
 *
 * You can obtain a copy of the license at usr/src/OPENSOLARIS.LICENSE
 * or http://www.opensolaris.org/os/licensing.
 * See the License for the specific language governing permissions
 * and limitations under the License.
 *
 * When distributing Covered Code, include this CDDL HEADER in each
 * file and include the License file at usr/src/OPENSOLARIS.LICENSE.
 * If applicable, add the following below this CDDL HEADER, with the
 * fields enclosed by brackets "[]" replaced with your own identifying
 * information: Portions Copyright [yyyy] [name of copyright owner]
 *
 * CDDL HEADER END
 */
/*
 * Copyright (c) 2021 by Delphix. All rights reserved.
 */

#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <libzutil.h>
#include <libnvpair.h>
#include <sys/vdev_impl.h>
#include <sys/vdev_object_store.h>

#include "zutil_zoa.h"

struct sockaddr_un zfs_public_socket = {
	AF_UNIX, "/etc/zfs/zfs_public_socket"
};

static struct sockaddr_un zfs_root_socket = {
	AF_UNIX, "/etc/zfs/zfs_root_socket"
};

static int
read_all(int fd, void *buf, size_t len)
{
	size_t read_total = 0;
	while (read_total < len) {
		size_t rc = read(fd, buf + read_total, len - read_total);
		if (rc > 0) {
			read_total += rc;
		} else if (rc < 0) {
			return (rc);
		}
	}
	return (read_total);
}

static struct sockaddr *
get_zfs_socket(zoa_socket_t zoa_sock)
{
	switch (zoa_sock) {
		case ZFS_PUBLIC_SOCKET:
			return ((struct sockaddr *)&zfs_public_socket);
			break;
		case ZFS_ROOT_SOCKET:
			return ((struct sockaddr *)&zfs_root_socket);
			break;
		default:
			ASSERT(0);
			return (NULL);
	}
}

nvlist_t *
zoa_send_recv_msg(nvlist_t *msg, zoa_socket_t zoa_sock)
{
	int sock = socket(AF_UNIX, SOCK_STREAM, 0);
	int err = connect(sock, get_zfs_socket(zoa_sock),
	    sizeof (struct sockaddr_un));
	if (err != 0) {
		fnvlist_free(msg);
		close(sock);
		return (NULL);
	}

	size_t len;
	char *buf = fnvlist_pack(msg, &len);
	fnvlist_free(msg);

	uint64_t len_le = htole64(len);
	ssize_t rv = write(sock, &len_le, sizeof (len_le));
	if (rv < 0) {
		fnvlist_pack_free(buf, len);
		close(sock);
		return (NULL);
	}
	ASSERT3U(rv, ==, sizeof (len_le));

	rv = write(sock, buf, len);
	fnvlist_pack_free(buf, len);

	if (rv < 0) {
		close(sock);
		return (NULL);
	}
	VERIFY3U(rv, ==, len); // XXX We need to handle partial writes here.

	uint64_t resp_size;
	size_t size;
	rv = read_all(sock, &resp_size, sizeof (resp_size));
	if (rv < 0) {
		close(sock);
		return (NULL);
	}
	VERIFY3U(rv, ==, sizeof (resp_size));

	size = le64toh(resp_size);
	buf = malloc(size);

	rv = read_all(sock, buf, size);
	close(sock);
	if (rv < 0) {
		free(buf);
		return (NULL);
	}
	ASSERT3U(rv, ==, size);

	nvlist_t *resp = fnvlist_unpack(buf, size);
	free(buf);

	return (resp);
}

struct destroying_pool {
	char *name;
	uint64_t guid;
	char *endpoint;
	char *bucket;
	uint64_t start_time;
	uint64_t total_data_objects;
	uint64_t destroyed_objects;
	boolean_t destroyed;
};

/*
 * Callback function for printing the status of a pool that is being destroyed.
 */
static boolean_t
print_destroying_item(struct destroying_pool item)
{
	time_t tsec;
	char tbuf[64] = "";
	struct tm t;

	if (item.start_time != 0) {
		tsec = (time_t)item.start_time;
		(void) localtime_r(&tsec, &t);
		(void) strftime(tbuf, sizeof (tbuf), "%F.%T", &t);
	}
	uint64_t pct = item.total_data_objects == 0 ? 0 :
	    ((double)item.destroyed_objects / item.total_data_objects) * 100;
	char *state = item.destroyed ? "DESTROYED" : "DESTROYING";

	// Print pool status.
	printf("\n  pool: %s", item.name);
	printf("\n  guid: %lu\n", item.guid);
	printf(" state: %s\n", state);
	if (item.destroyed) {
		printf("status: The pool has been destroyed.\n");
		if (item.start_time != 0) {
			printf("        zpool destroy was initiated at %s UTC "
			    "and is complete.\n", tbuf);
		}
	} else {
		printf("status: The pool is being destroyed.\n");
		if (item.start_time != 0) {
			printf("        zpool destroy was initiated at %s UTC "
			    "and is %lu%% complete.\n", tbuf, pct);
		}
	}
	printf("config:\n\n");
	printf("        NAME                 STATE\n");
	printf("        %-20s %s\n", item.name,  state);
	printf("          %s:%s %s\n", item.endpoint, item.bucket, state);

	return (B_TRUE);
}

static void
zoa_list_destroy_pools(boolean_t destroy_complete)
{
	nvlist_t *msg = fnvlist_alloc();
	fnvlist_add_string(msg, AGENT_TYPE, AGENT_TYPE_GET_DESTROYING_POOLS);

	nvlist_t *resp = zoa_send_recv_msg(msg, ZFS_PUBLIC_SOCKET);
	if (resp == NULL)
		return;

	const char *type = fnvlist_lookup_string(resp, AGENT_TYPE);
	VERIFY0(strcmp(type, AGENT_TYPE_GET_DESTROYING_POOLS_DONE));

	nvlist_t *nvpools = NULL;
	(void) nvlist_lookup_nvlist(resp, AGENT_POOLS, &nvpools);


	nvpair_t *elem = NULL;
	struct destroying_pool item = { 0 };
	while ((elem = nvlist_next_nvpair(nvpools, elem)) != NULL) {
		nvlist_t *config;
		VERIFY0(nvpair_value_nvlist(elem, &config));

		item.destroyed = fnvlist_lookup_boolean_value(config,
		    AGENT_DESTROY_DOMPLETED);
		if (item.destroyed != destroy_complete) {
			continue;
		}

		item.name = fnvlist_lookup_string(config, AGENT_NAME);
		item.guid =
		    fnvlist_lookup_uint64(config, AGENT_GUID);
		item.bucket = fnvlist_lookup_string(config, AGENT_BUCKET);
		item.endpoint = fnvlist_lookup_string(config, AGENT_ENDPOINT);
		// Optional componenents
		(void) nvlist_lookup_uint64(config, AGENT_START_TIME,
		    &item.start_time);
		(void) nvlist_lookup_uint64(config, AGENT_TOTAL_DATA_OBJECTS,
		    &item.total_data_objects);
		(void) nvlist_lookup_uint64(config, AGENT_DESTROYED_OBJECTS,
		    &item.destroyed_objects);

		print_destroying_item(item);
	}

	fnvlist_free(resp);
}

/*
 * Print a status message for pools that have been completely destroyed are
 * also included.
 */

void
zoa_list_destroyed_pools(void)
{
	zoa_list_destroy_pools(B_TRUE);
}

/*
 * Print a status message for pools that are being destroyed.
 */
void
zoa_list_destroying_pools(void)
{
	zoa_list_destroy_pools(B_FALSE);
}

/*
 * Clear the destroyed pools so that they are not listed going forward.
 */
void
zoa_clear_destroyed_pools(void)
{
	nvlist_t *msg = fnvlist_alloc();
	fnvlist_add_string(msg, AGENT_TYPE, AGENT_TYPE_CLEAR_DESTROYED_POOLS);

	zoa_send_recv_msg(msg, ZFS_PUBLIC_SOCKET);
}
