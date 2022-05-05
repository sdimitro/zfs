/*
 * CDDL HEADER START
 *
 * This file and its contents are supplied under the terms of the
 * Common Development and Distribution License ("CDDL"), version 1.0.
 * You may only use this file in accordance with the terms of version
 * 1.0 of the CDDL.
 *
 * A full copy of the text of the CDDL should have accompanied this
 * source.  A copy of the CDDL is also available via the Internet at
 * http://www.illumos.org/license/CDDL.
 *
 * CDDL HEADER END
 */
/*
 * Copyright (c) 2021, 2022 by Delphix. All rights reserved.
 */

#include <sys/zfs_context.h>
#include <sys/errno.h>
#include <sys/spa.h>
#include <sys/spa_impl.h>
#include <sys/vdev_impl.h>
#include <sys/vdev_trim.h>
#include <sys/vdev_object_store.h>
#include <sys/zio.h>
#include <sys/fs/zfs.h>
#include <sys/fm/fs/zfs.h>
#include <sys/abd.h>
#include <sys/metaslab_impl.h>
#include <sys/sock.h>
#include <sys/zap.h>

/*
 * Virtual device vector for object storage.
 */

/*
 * By default, the logical/physical ashift for object store vdevs is set to
 * SPA_MINBLOCKSHIFT (9). This allows all object store vdevs to use
 * 512B (1 << 9) blocksizes. Users may opt to change one or both of these
 * for testing or performance reasons. Care should be taken as these
 * values will impact the vdev_ashift setting which can only be set at
 * vdev creation time.
 */
unsigned long vdev_object_store_logical_ashift = SPA_MINBLOCKSHIFT;
unsigned long vdev_object_store_physical_ashift = SPA_MINBLOCKSHIFT;

unsigned long vdev_object_store_max_payload = SPA_MAXBLOCKSIZE;

struct sockaddr_un zfs_root_socket = {
	AF_UNIX, "/etc/zfs/zfs_root_socket"
};

/*
 * Free in batches of 100,000 blocks, to limit memory usage.  Note that
 * the Agent accepts requests of up to ~20MB, and each block uses 12 bytes,
 * so the max "free blocks" request is ~1.2MB.
 */
int vdev_object_store_max_frees = 100000;

/*
 * Counters for tracking partial writes and retries.
 */
static uint64_t partial_write_counter = 0;
static uint64_t write_retry_counter = 0;

/* Taskq used for agent_resume. */
taskq_t *resume_taskq;

typedef enum {
	VOS_SOCK_UNINITIALIZED = 0,
	VOS_SOCK_CLOSED = (1 << 0),
	VOS_SOCK_SHUTDOWN = (1 << 1),
	VOS_SOCK_OPENING = (1 << 2),
	VOS_SOCK_OPEN = (1 << 3),
	VOS_SOCK_READY = (1 << 4)
} socket_state_t;

typedef enum {
	VOS_TXG_BEGIN = 0,
	VOS_TXG_END,
	VOS_TXG_END_AGAIN,
	VOS_TXG_NONE
} vos_serial_flag_t;

typedef enum {
	VOS_SERIAL_CREATE_POOL,
	VOS_SERIAL_OPEN_POOL,
	VOS_SERIAL_END_TXG,
	VOS_SERIAL_CLOSE_POOL,
	VOS_SERIAL_ENABLE_FEATURE,
	VOS_SERIAL_TYPES
} vos_serial_types_t;

typedef enum {
	VOS_RESUME_NOT_RUNNING = 0,
	VOS_RESUME_STARTING = (1 << 0),
	VOS_RESUME_START = (1 << 1),
	VOS_RESUME_OPENING = (1 << 2),
	VOS_RESUME_OPENED = (1 << 3),
	VOS_RESUME_REISSUE = (1 << 4),
	VOS_RESUME_FAILED = (1 << 5)
} agent_resume_state_t;

typedef struct object_store_free_block {
	list_node_t osfb_list_node;
	uint64_t osfb_offset;
	uint64_t osfb_size;
} object_store_free_block_t;


typedef struct vdev_object_store {
	vdev_t *vos_vdev;
	char *vos_protocol;
	char *vos_endpoint;
	char *vos_region;
	char *vos_cred_profile;
	kthread_t *vos_agent_thread;
	kmutex_t vos_lock;
	kcondvar_t vos_cv;
	boolean_t vos_agent_thread_exit;

	kmutex_t vos_stats_lock; /* protects vos_stats and avl tree */
	vdev_object_store_stats_t vos_stats;
	avl_tree_t vos_pending_stats_tree;

	/*
	 * The vos_sock_lock protects the vos_sock_state.
	 * It is held when setting a new state or waiting
	 * for a specific state to be present.
	 */
	kmutex_t vos_sock_lock;
	socket_state_t vos_sock_state;
	kcondvar_t vos_sock_cv;

	/*
	 * The vos_sock_rwlock must be held as READER when
	 * issuing a request or shutting down the socket.
	 * It must be held as WRITER when closing or opening
	 * the socket.
	 */
	krwlock_t vos_sock_rwlock;
	ksocket_t vos_sock;

	/* updated atomically */
	uint64_t vos_max_blockid;

	kmutex_t vos_outstanding_lock;
	kcondvar_t vos_outstanding_cv;
	boolean_t vos_serial_done[VOS_SERIAL_TYPES];
	vos_serial_flag_t vos_send_txg_selector;
	boolean_t vos_open_completed;
	boolean_t vos_create_completed;
	boolean_t vos_closing;
	const char *vos_feature_enable;
	uint64_t vos_result;

	kmutex_t vos_resume_lock;
	kcondvar_t vos_resume_cv;
	agent_resume_state_t vos_resume_state;

	uint64_t vos_next_block;
	uberblock_t vos_uberblock;
	nvlist_t *vos_config;

	list_t vos_free_list;
	uint64_t vos_free_list_len;

	uint64_t vos_version_major;
	uint64_t vos_version_minor;
	uint64_t vos_version_patch;
} vdev_object_store_t;

/*
 * Kernel context for vdev_object_store_stats_generate() calls
 */
typedef struct object_store_stats_call {
	kmutex_t	oss_lock;
	kcondvar_t	oss_cv;
	uint64_t	oss_owner;
	nvlist_t	*oss_nvl;
	avl_node_t	oss_node;
} object_store_stats_call_t;

static mode_t
vdev_object_store_open_mode(spa_mode_t spa_mode)
{
	mode_t mode = 0;

	if ((spa_mode & SPA_MODE_READ) && (spa_mode & SPA_MODE_WRITE)) {
		mode = O_RDWR;
	} else if (spa_mode & SPA_MODE_READ) {
		mode = O_RDONLY;
	} else {
		panic("unknown spa mode");
	}

	return (mode);
}

/*
 * Helper routines when manipulating the socket connection to the
 * agent. The vos_sock_rwlock is held when using or manipulating the
 * socket.
 */

static void
zfs_object_store_wait(vdev_object_store_t *vos, socket_state_t state)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	ASSERT(MUTEX_NOT_HELD(&vos->vos_outstanding_lock));
	while (vos->vos_sock_state < state) {
		cv_wait(&vos->vos_sock_cv, &vos->vos_sock_lock);
	}
}

static void
zfs_object_store_shutdown(vdev_object_store_t *vos)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));

	/*
	 * We are disconnecting from the socket. We only
	 * need to hold the lock as READER since the socket
	 * is still valid and shutting it down will wakeup
	 * any readers or writers currently using the socket.
	 * Any thread that tries to use the socket after it is
	 * shutdown will get an error.
	 */
	rw_enter(&vos->vos_sock_rwlock, RW_READER);
	if (vos->vos_sock == INVALID_SOCKET) {
		rw_exit(&vos->vos_sock_rwlock);
		return;
	}
	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE_SOCKET) {
		zfs_dbgmsg("SOCKET SHUTTING DOWN(%px): " SOCK_FMT, curthread,
		    vos->vos_sock);
	}
	ksock_shutdown(vos->vos_sock, SHUT_RDWR);
	rw_exit(&vos->vos_sock_rwlock);
	vos->vos_sock_state = VOS_SOCK_SHUTDOWN;
}

static void
zfs_object_store_close(vdev_object_store_t *vos)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	rw_enter(&vos->vos_sock_rwlock, RW_WRITER);
	vos->vos_sock_state = VOS_SOCK_CLOSED;
	if (vos->vos_sock == INVALID_SOCKET) {
		rw_exit(&vos->vos_sock_rwlock);
		return;
	}

	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE_SOCKET) {
		zfs_dbgmsg("SOCKET CLOSING(%px): " SOCK_FMT,
		    curthread, vos->vos_sock);
	}
	ksock_close(vos->vos_sock);
	vos->vos_sock = INVALID_SOCKET;
	rw_exit(&vos->vos_sock_rwlock);
}

static int
zfs_object_store_open(vdev_object_store_t *vos)
{
	ksocket_t s = INVALID_SOCKET;

	mutex_enter(&vos->vos_sock_lock);
	vos->vos_sock_state = VOS_SOCK_OPENING;
	mutex_exit(&vos->vos_sock_lock);
	int rc = ksock_create(PF_UNIX, SOCK_STREAM, 0, &s);
	if (rc != 0) {
		zfs_dbgmsg("zfs_object_store_open unable to create "
		    "socket: %d", rc);
		return (rc);
	}

	rc = ksock_connect(s, (struct sockaddr *)&zfs_root_socket,
	    sizeof (zfs_root_socket));
	if (rc != 0) {
		/*
		 * We failed to connect probably because the agent
		 * is restarting. We don't treat this as an error
		 * since the caller will retry the operation if
		 * it finds that the socket is still invalid.
		 */
		zfs_dbgmsg("zfs_object_store_open failed to "
		    "connect: %d", rc);
		ksock_close(s);
		s = INVALID_SOCKET;
	} else {
		zfs_dbgmsg("zfs_object_store_open, socket connection "
		    "ready, " SOCK_FMT, s);
	}

	rw_enter(&vos->vos_sock_rwlock, RW_WRITER);
	VERIFY3P(vos->vos_sock, ==, INVALID_SOCKET);
	vos->vos_sock = s;
	rw_exit(&vos->vos_sock_rwlock);

	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE_SOCKET) {
		zfs_dbgmsg("SOCKET OPEN(%px): " SOCK_FMT, curthread,
		    vos->vos_sock);
	}
	return (0);
}

static ssize_t
zfs_object_store_receive(vdev_object_store_t *vos, kvec_t *iov,
    int iovcnt, int size, int flags)
{
	struct msghdr msg = {};
	rw_enter(&vos->vos_sock_rwlock, RW_READER);
	if (vos->vos_sock == INVALID_SOCKET) {
		zfs_dbgmsg("(%px) zfs_object_store_receive socket closed",
		    curthread);
		rw_exit(&vos->vos_sock_rwlock);
		return (SET_ERROR(-ENOTCONN));
	}

	ssize_t recvd = ksock_receive(vos->vos_sock, &msg, iov, iovcnt,
	    size, flags);
	rw_exit(&vos->vos_sock_rwlock);
	return (recvd);
}

static ssize_t
zfs_object_store_send(vdev_object_store_t *vos, kvec_t *iov, int iovcnt,
    int size)
{
	struct msghdr msg = {};
	rw_enter(&vos->vos_sock_rwlock, RW_READER);
	if (vos->vos_sock == INVALID_SOCKET) {
		zfs_dbgmsg("(%px) zfs_object_store_send socket closed",
		    curthread);
		rw_exit(&vos->vos_sock_rwlock);
		return (SET_ERROR(-ENOTCONN));
	}
	ssize_t sent = ksock_send(vos->vos_sock, &msg, iov, iovcnt, size);
	rw_exit(&vos->vos_sock_rwlock);
	return (sent);
}

static int
agent_read_all(vdev_object_store_t *vos, void *buf, size_t len)
{
	boolean_t locked = MUTEX_HELD(&vos->vos_lock);
	size_t recvd_total = 0;
	while (recvd_total < len) {
		kvec_t iov = {};

		iov.iov_base = buf + recvd_total;
		iov.iov_len = len - recvd_total;

		if (!locked)
			mutex_enter(&vos->vos_lock);
		if (vos->vos_agent_thread_exit ||
		    vos->vos_sock == INVALID_SOCKET) {
			zfs_dbgmsg("(%px) agent_read_all shutting down",
			    curthread);
			if (!locked)
				mutex_exit(&vos->vos_lock);
			return (SET_ERROR(ENOTCONN));
		}

		if (!locked)
			mutex_exit(&vos->vos_lock);

		ssize_t recvd = zfs_object_store_receive(vos,
		    &iov, 1, len - recvd_total, 0);
		if (recvd > 0) {
			recvd_total += recvd;
			if (recvd_total < len &&
			    (zfs_flags & ZFS_DEBUG_OBJECT_STORE)) {
				zfs_dbgmsg("incomplete recvmsg but trying for "
				    "more len=%zu recvd=%zd  "
				    "recvd_total=%zu",
				    len, recvd, recvd_total);
			}
		} else {
			zfs_dbgmsg("got wrong length from agent socket: "
			    "for total size %zu, already received %zu, "
			    "expected up to %zu got %zd",
			    len, recvd_total, (len - recvd_total), recvd);
			/* XXX - Do we need to check for errors too? */
			if (recvd == 0)
				return (SET_ERROR(EAGAIN));
		}
	}
	return (0);
}

static int
agent_read_nvlist(vdev_object_store_t *vos, nvlist_t **out)
{
	message_header_t header;
	int err = agent_read_all(vos, &header, sizeof (header));
	if (err != 0) {
		zfs_dbgmsg("agent_read_nvlist(%px) got err %d", curthread, err);
		return (err);
	}
	if (header.message_type != MESSAGE_NVLIST) {
		zfs_dbgmsg("agent_read_nvlist(%px) invalid message_type %u",
		    curthread, header.message_type);
		return (EINVAL);
	}
	if (header.payload_len > SPA_MAXBLOCKSIZE) {
		zfs_dbgmsg("agent_read_nvlist(%px) got invalid payload_len %x",
		    curthread, header.payload_len);
		return (EINVAL);
	}

	void *buf = vmem_alloc(header.payload_len, KM_SLEEP);
	err = agent_read_all(vos, buf, header.payload_len);
	if (err != 0) {
		zfs_dbgmsg("2 agent_read_nvlist(%px) got err %d", curthread,
		    err);
		vmem_free(buf, header.payload_len);
		return (err);
	}

	err = nvlist_unpack(buf, header.payload_len, out, KM_SLEEP);
	vmem_free(buf, header.payload_len);
	if (err != 0) {
		zfs_dbgmsg("got error %d from nvlist_unpack(len=%d)",
		    err, header.payload_len);
		return (EAGAIN);
	}
	return (0);
}

static int
agent_write_all(vdev_object_store_t *vos, message_header_t *header,
    void *struct_buf, void *payload_buf)
{
	size_t total_size = sizeof (*header) +
	    header->struct_len + header->payload_len;
	size_t write_total = 0;
	kvec_t iov[3] = {};
	boolean_t locked = MUTEX_HELD(&vos->vos_lock);

	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));

	while (write_total < total_size) {
		int iov_count;
		if (write_total < sizeof (*header)) {
			size_t already_written = write_total;
			iov_count = 3;
			iov[0].iov_base = ((char *)header) + already_written;
			iov[0].iov_len = sizeof (*header) - already_written;
			iov[1].iov_base = struct_buf;
			iov[1].iov_len = header->struct_len;
			iov[2].iov_base = payload_buf;
			iov[2].iov_len = header->payload_len;
		} else if (write_total <
		    sizeof (*header) + header->struct_len) {
			size_t already_written =
			    write_total - sizeof (*header);
			iov_count = 2;
			iov[0].iov_base = struct_buf + already_written;
			iov[0].iov_len = header->struct_len - already_written;
			iov[1].iov_base = payload_buf;
			iov[1].iov_len = header->payload_len;
		} else {
			size_t already_written =
			    write_total - sizeof (*header) - header->struct_len;
			iov_count = 1;
			iov[0].iov_base = payload_buf + already_written;
			iov[0].iov_len =
			    header->payload_len - already_written;
		}

		if (!locked)
			mutex_enter(&vos->vos_lock);
		if (vos->vos_agent_thread_exit ||
		    vos->vos_sock == INVALID_SOCKET) {
			zfs_dbgmsg("(%px) agent_write_all shutting down",
			    curthread);
			if (!locked)
				mutex_exit(&vos->vos_lock);
			return (SET_ERROR(ENOTCONN));
		}
		if (!locked)
			mutex_exit(&vos->vos_lock);

		ssize_t sent;
		do {
			sent = zfs_object_store_send(vos, iov,
			    iov_count, total_size - write_total);
			if (sent < 0) {
				if (sent == -ERESTARTSYS) {
					write_retry_counter++;
					zfs_dbgmsg("got ERESTARTSYS "
					    "writing to socket, "
					    "write_retry_counter: %llu",
					    (u_longlong_t)write_retry_counter);
				} else {
					zfs_dbgmsg("error sending message to "
					    "agent socket: %zd", sent);
				}
			}
		} while (sent == -ERESTARTSYS);
		if (sent > 0) {
			write_total += sent;
			if ((write_total < total_size) &&
			    (zfs_flags & ZFS_DEBUG_OBJECT_STORE)) {
				partial_write_counter++;
				zfs_dbgmsg("incomplete ksock_send len=%zu "
				    "sent=%zd write_total=%zu "
				    "partial_write_counter=%llu",
				    total_size, sent, write_total,
				    (u_longlong_t)partial_write_counter);
			}
		} else if (sent == 0) {
			zfs_dbgmsg("agent restarted when writing len=%zu, "
			    "write_total=%zu", total_size, write_total);
			return (SET_ERROR(EAGAIN));
		} else {
			zfs_dbgmsg("error sending message to agent socket: "
			    "for total_size=%zu, got %zd",
			    total_size, sent);
			return (SET_ERROR(EAGAIN));
		}
	}
	return (0);
}

static int
agent_request(vdev_object_store_t *vos, message_type_t message_type,
    void *struct_buf, size_t struct_len,
    void *payload_buf, size_t payload_len, char *tag)
{
	spa_t *spa = vos->vos_vdev->vdev_spa;

	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));

	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
		zfs_dbgmsg("sending message_type=%u struct_len=%u "
		    "payload_len=%u",
		    (int)message_type,
		    (int)struct_len,
		    (int)payload_len);
	}

	if (vos->vos_sock_state < VOS_SOCK_OPENING) {
		return (SET_ERROR(ENOTCONN));
	}

	if (zio_injection_enabled) {
		zfs_dbgmsg("%s INJECTION prior to send", tag);
		zio_handle_panic_injection(spa, tag, 1);
	}

	message_header_t header = {
		.message_type = message_type,
		.struct_len = struct_len,
		.payload_len = payload_len,
	};

	int err = agent_write_all(vos, &header, struct_buf, payload_buf);
	if (err != 0) {
		zfs_dbgmsg("agent_request(%px) got err %d", curthread, err);

		/*
		 * If we were unable to send, then the kernel
		 * will shutdown the socket and allow the resume
		 * logic to re-establish the connection and retry
		 * any operations which were in flight prior to this
		 * failure. We only shutdown the socket here and
		 * allow the vdev_agent_thread to handle the closing
		 * and reopening logic.
		 */
		zfs_object_store_shutdown(vos);
	}

	if (zio_injection_enabled) {
		zfs_dbgmsg("%s INJECTION after send", tag);
		zio_handle_panic_injection(spa, tag, 2);
	}

	return (err != 0 ? SET_ERROR(EINTR) : 0);
}

static int
agent_request_nv(vdev_object_store_t *vos, nvlist_t *nv, char *tag)
{
	size_t payload_len;
	void *payload_buf = fnvlist_pack(nv, &payload_len);
	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
		zfs_dbgmsg("sending %llu-byte request to agent nvlist type=%s",
		    (u_longlong_t)payload_len,
		    fnvlist_lookup_string(nv, AGENT_REQUEST_TYPE));
	}
	int err = agent_request(vos, MESSAGE_NVLIST, NULL, 0,
	    payload_buf, payload_len, tag);
	fnvlist_pack_free(payload_buf, payload_len);
	return (err);
}

static int
agent_request_read(vdev_object_store_t *vos, read_block_request_t *req)
{
	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
		zfs_dbgmsg("agent_request_read(blockid=%llu)",
		    (u_longlong_t)req->block);
	}
	return (agent_request(vos, MESSAGE_READ_BLOCK, req, sizeof (*req),
	    NULL, 0, FTAG));
}

static int
agent_request_write(vdev_object_store_t *vos, write_block_request_t *req,
    void *buf, size_t len)
{
	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
		zfs_dbgmsg("agent_request_write(blockid=%llu)",
		    (u_longlong_t)req->block);
	}
	return (agent_request(vos, MESSAGE_WRITE_BLOCK, req, sizeof (*req),
	    buf, len, FTAG));
}

static int
agent_request_serial(vdev_object_store_t *vos, nvlist_t *nv, char *tag,
    vos_serial_types_t wait_type)
{
	ASSERT(!vos->vos_serial_done[wait_type]);
	return (agent_request_nv(vos, nv, tag));
}

/*
 * Send request to agent; nvlist may be modified.
 */
static int
agent_request_zio(vdev_object_store_t *vos, zio_t *zio)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));

	uint64_t blockid = zio->io_offset >> SPA_MINBLOCKSHIFT;

	int err = 0;
	if (zio->io_type == ZIO_TYPE_WRITE) {
		write_block_request_t req = {
			.block = blockid,
			.token = (uintptr_t)zio,
		};
		void *buf = abd_borrow_buf_copy(zio->io_abd, zio->io_size);
		err = agent_request_write(vos, &req, buf, zio->io_size);
		abd_return_buf(zio->io_abd, buf, zio->io_size);
	} else if (zio->io_type == ZIO_TYPE_READ) {
		read_block_request_t req = {
			.block = blockid,
			.token = (uintptr_t)zio,
			.heal = ((zio->io_flags & ZIO_FLAG_IO_RETRY) ||
			    (zio->io_flags & ZIO_FLAG_SCRUB)),
			.size = zio->io_size,
		};
		err = agent_request_read(vos, &req);
	} else {
		ASSERT(!"invalid io_type");
	}
	return (err);
}

static zio_t *
agent_complete_zio(vdev_object_store_t *vos, uintptr_t token)
{
	vdev_t *vd = vos->vos_vdev;
	vdev_queue_t *vq = &vd->vdev_queue;

	mutex_enter(&vq->vq_lock);
	zio_t *zio = avl_find(&vq->vq_active_tree, (zio_t *)token, NULL);
	VERIFY3P(zio, !=, NULL);
	VERIFY3P(zio, ==, token);

	vdev_queue_pending_remove(vq, zio);
	mutex_exit(&vq->vq_lock);

	return (zio);
}

/*
 * Wait for a one-at-a-time operation to complete
 * (pool create, pool open, txg end). If there was an
 * error with the socket, threads will wait here and we will
 * retry the operation.
 */
static void
agent_wait_serial(vdev_object_store_t *vos, vos_serial_types_t wait_type)
{
	ASSERT(!MUTEX_HELD(&vos->vos_sock_lock));
	mutex_enter(&vos->vos_outstanding_lock);
	while (!vos->vos_serial_done[wait_type])
		cv_wait(&vos->vos_outstanding_cv, &vos->vos_outstanding_lock);
	vos->vos_serial_done[wait_type] = B_FALSE;
	mutex_exit(&vos->vos_outstanding_lock);
}

static void
agent_serial_done(vdev_object_store_t *vos, vos_serial_types_t wait_type)
{
	mutex_enter(&vos->vos_outstanding_lock);
	ASSERT(!vos->vos_serial_done[wait_type]);
	vos->vos_serial_done[wait_type] = B_TRUE;
	cv_broadcast(&vos->vos_outstanding_cv);
	mutex_exit(&vos->vos_outstanding_lock);
}

static int
agent_free_blocks_impl(vdev_object_store_t *vos,
    uint64_t *blkids, uint32_t *sizes, int num)
{
	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_FREE_BLOCKS);
	fnvlist_add_uint64_array(nv, AGENT_BLOCK, blkids, num);
	fnvlist_add_uint32_array(nv, AGENT_SIZE, sizes, num);
	int err = agent_request_nv(vos, nv, FTAG);
	fnvlist_free(nv);
	if (err == 0) {
		zfs_dbgmsg("agent_free_blocks freed %d blocks", num);
	} else {
		zfs_dbgmsg("agnet_free_blocks failed to send: %d", err);
	}
	return (err);
}

static int
agent_free_blocks(vdev_object_store_t *vos)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));

	int buf_len = MIN(vos->vos_free_list_len, vdev_object_store_max_frees);
	if (buf_len == 0)
		return (0);
	int err = 0;

	uint64_t *blkid_array =
	    vmem_alloc(buf_len * sizeof (*blkid_array), KM_SLEEP);
	uint32_t *size_array =
	    vmem_alloc(buf_len * sizeof (*size_array), KM_SLEEP);

	int num_freed = 0;
	for (object_store_free_block_t *osfb = list_head(&vos->vos_free_list);
	    osfb != NULL; osfb = list_next(&vos->vos_free_list, osfb)) {
		uint64_t blockid = osfb->osfb_offset >> 9;
		blkid_array[num_freed] = blockid;
		size_array[num_freed] = osfb->osfb_size;
		num_freed++;

		if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
			zfs_dbgmsg("agent_free_blocks(blkid=%llu, asize=%llu)",
			    (u_longlong_t)blockid,
			    (u_longlong_t)osfb->osfb_size);
		}

		if (num_freed == buf_len) {
			err = agent_free_blocks_impl(vos,
			    blkid_array, size_array, num_freed);
			num_freed = 0;
			if (err != 0)
				break;
		}
	}
	if (num_freed != 0) {
		err = agent_free_blocks_impl(vos,
		    blkid_array, size_array, num_freed);
	}
	vmem_free(blkid_array, buf_len * sizeof (*blkid_array));
	vmem_free(size_array, buf_len * sizeof (*size_array));
	return (err);
}

static void
agent_close_pool(vdev_object_store_t *vos, boolean_t destroy)
{
	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_CLOSE_POOL);
	fnvlist_add_boolean_value(nv, AGENT_DESTROY, destroy);
	agent_request_serial(vos, nv, FTAG, VOS_SERIAL_CLOSE_POOL);
	vos->vos_closing = B_TRUE;
	mutex_exit(&vos->vos_sock_lock);
	fnvlist_free(nv);
}

static void
agent_create_pool(vdev_t *vd, vdev_object_store_t *vos)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	/*
	 * We need to ensure that we only issue a request when the
	 * socket is ready. Otherwise, we block here since the agent
	 * might be in recovery.
	 */
	zfs_object_store_wait(vos, VOS_SOCK_OPEN);

	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_CREATE_POOL);
	fnvlist_add_string(nv, AGENT_NAME, spa_name(vd->vdev_spa));
	fnvlist_add_uint64(nv, AGENT_GUID, spa_guid(vd->vdev_spa));
	if (vos->vos_cred_profile != NULL) {
		fnvlist_add_string(nv, AGENT_CRED_PROFILE,
		    vos->vos_cred_profile);
	}
	fnvlist_add_string(nv, AGENT_PROTOCOL, vos->vos_protocol);
	if (vos->vos_endpoint != NULL)
		fnvlist_add_string(nv, AGENT_ENDPOINT, vos->vos_endpoint);
	if (vos->vos_region != NULL)
		fnvlist_add_string(nv, AGENT_REGION, vos->vos_region);
	fnvlist_add_string(nv, AGENT_BUCKET, vd->vdev_path);
	zfs_dbgmsg("agent_create_pool(guid=%llu name=%s bucket=%s)",
	    (u_longlong_t)spa_guid(vd->vdev_spa),
	    spa_name(vd->vdev_spa),
	    vd->vdev_path);
	agent_request_serial(vos, nv, FTAG, VOS_SERIAL_CREATE_POOL);

	fnvlist_free(nv);
}

static uint64_t
agent_open_pool(vdev_t *vd, vdev_object_store_t *vos, mode_t mode,
    boolean_t resume)
{
	/*
	 * We need to ensure that we only issue a request when the
	 * socket is ready. Otherwise, we block here since the agent
	 * might be in recovery.
	 */
	mutex_enter(&vos->vos_sock_lock);
	zfs_object_store_wait(vos, VOS_SOCK_OPEN);

	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_OPEN_POOL);
	fnvlist_add_uint64(nv, AGENT_GUID, spa_guid(vd->vdev_spa));
	if (vos->vos_cred_profile != NULL) {
		fnvlist_add_string(nv, AGENT_CRED_PROFILE,
		    vos->vos_cred_profile);
	}
	fnvlist_add_string(nv, AGENT_PROTOCOL, vos->vos_protocol);
	if (vos->vos_endpoint != NULL)
		fnvlist_add_string(nv, AGENT_ENDPOINT, vos->vos_endpoint);
	if (vos->vos_region != NULL)
		fnvlist_add_string(nv, AGENT_REGION, vos->vos_region);
	fnvlist_add_string(nv, AGENT_BUCKET, vd->vdev_path);
	fnvlist_add_boolean_value(nv, AGENT_ROLLBACK,
	    !!(vd->vdev_spa->spa_import_flags & ZFS_IMPORT_CHECKPOINT));
	if (mode == O_RDONLY)
		fnvlist_add_boolean(nv, AGENT_READONLY);
	if (vd->vdev_spa->spa_load_max_txg != UINT64_MAX) {
		fnvlist_add_uint64(nv, AGENT_TXG,
		    vd->vdev_spa->spa_load_max_txg);
	}

	/*
	 * When we're resuming from an agent restart and we're
	 * in the middle of a txg, then we need to let the agent
	 * know the txg value.
	 */
	if (resume && vos->vos_send_txg_selector <= VOS_TXG_END) {
		fnvlist_add_uint64(nv, AGENT_SYNCING_TXG,
		    spa_syncing_txg(vd->vdev_spa));
	}

	/*
	 * If we are in the resume path, and the initial open hasn't been
	 * completed, waiting will result in either this thread or the original
	 * thread hanging indefintely, and there's no way to know which will
	 * occur. Instead, we simply skip the wait_serial call if we're in that
	 * case.
	 */
	boolean_t wait = !resume || vos->vos_open_completed;
	zfs_dbgmsg("agent_open_pool(guid=%llu bucket=%s)",
	    (u_longlong_t)spa_guid(vd->vdev_spa),
	    vd->vdev_path);
	agent_request_serial(vos, nv, FTAG, VOS_SERIAL_OPEN_POOL);

	mutex_exit(&vos->vos_sock_lock);
	fnvlist_free(nv);
	if (wait)
		agent_wait_serial(vos, VOS_SERIAL_OPEN_POOL);
	return (vos->vos_result);
}

static void
agent_begin_txg(vdev_object_store_t *vos, uint64_t txg)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	zfs_object_store_wait(vos, VOS_SOCK_READY);

	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_BEGIN_TXG);
	fnvlist_add_uint64(nv, AGENT_TXG, txg);
	zfs_dbgmsg("agent_begin_txg(%llu)",
	    (u_longlong_t)txg);

	agent_request_nv(vos, nv, FTAG);
	fnvlist_free(nv);
}

static void
agent_resume_complete(vdev_object_store_t *vos)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	zfs_object_store_wait(vos, VOS_SOCK_OPEN);

	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_RESUME_COMPLETE);

	zfs_dbgmsg("agent_resume_complete()");
	agent_request_nv(vos, nv, FTAG);
	fnvlist_free(nv);
}

static void
agent_end_txg(vdev_object_store_t *vos, uint64_t txg, void *ub_buf,
    size_t ub_len, void *config_buf, size_t config_len)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	/*
	 * External consumers need to wait until the connection has
	 * reached a ready state. However, when we are doing recovery
	 * we only need to be in the open state, so we check that here.
	 */
	zfs_object_store_wait(vos, VOS_SOCK_OPEN);

	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_END_TXG);
	fnvlist_add_uint64(nv, AGENT_TXG, txg);
	fnvlist_add_uint8_array(nv, AGENT_UBERBLOCK, ub_buf, ub_len);
	fnvlist_add_uint8_array(nv, AGENT_CONFIG, config_buf, config_len);
	fnvlist_add_uint64(nv, AGENT_CHECKPOINT,
	    vos->vos_vdev->vdev_spa->spa_checkpoint_txg);

	zfs_dbgmsg("agent_end_txg(%llu), %u passes",
	    (u_longlong_t)txg,
	    vos->vos_vdev->vdev_spa->spa_sync_pass);
	agent_request_serial(vos, nv, FTAG, VOS_SERIAL_END_TXG);
	fnvlist_free(nv);
}

static void
agent_flush_writes(vdev_object_store_t *vos, uint64_t blockid)
{
	mutex_enter(&vos->vos_sock_lock);
	zfs_object_store_wait(vos, VOS_SOCK_READY);

	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_FLUSH_WRITES);
	fnvlist_add_uint64(nv, AGENT_BLOCK, blockid);
	zfs_dbgmsg("agent_flush: blockid %llu", (u_longlong_t)blockid);

	agent_request_nv(vos, nv, FTAG);
	mutex_exit(&vos->vos_sock_lock);
	fnvlist_free(nv);
}

static void
agent_set_feature(vdev_object_store_t *vos, const char *guid)
{
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	zfs_object_store_wait(vos, VOS_SOCK_OPEN);

	nvlist_t *nv = fnvlist_alloc();
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_ENABLE_FEATURE);
	fnvlist_add_string(nv, AGENT_FEATURE, guid);
	zfs_dbgmsg("agent_set_feature: feature %s", guid);

	/*
	 * We do a serial operation here because we need to make sure that a
	 * response is waited for before we proceed with the txg and
	 * potentially finish it. This may be better suited for the upcoming
	 * token-based approach planned for iostat.
	 */
	agent_request_serial(vos, nv, FTAG, VOS_SERIAL_ENABLE_FEATURE);
	vos->vos_feature_enable = guid;
	mutex_exit(&vos->vos_sock_lock);
	fnvlist_free(nv);
}

void
object_store_restart_agent(vdev_t *vd)
{
	vdev_object_store_t *vos = vd->vdev_tsd;
	ASSERT(MUTEX_HELD(&vos->vos_sock_lock));
	/*
	 * We need to ensure that we only issue a request when the
	 * socket is ready. Otherwise, we block here since the agent
	 * might be in recovery.
	 */
	zfs_object_store_wait(vos, VOS_SOCK_OPEN);

	nvlist_t *nv = fnvlist_alloc();
	/*
	 * XXX This doesn't actually exit the agent, it just tells the agent to
	 * close the connection.  We could just as easily close the connection
	 * ourself.  Or change the agent code to actually exit.
	 */
	fnvlist_add_string(nv, AGENT_REQUEST_TYPE, AGENT_TYPE_EXIT);
	agent_request_nv(vos, nv, FTAG);
	fnvlist_free(nv);
}

/*
 * XXX This doesn't actually stop the agent, it just tells the agent to close
 * the pool (practically, to mark the pool as no longer owned by this agent).
 */
static void
object_store_stop_agent(vdev_t *vd)
{
	vdev_object_store_t *vos = vd->vdev_tsd;
	mutex_enter(&vos->vos_sock_lock);
	if (vos->vos_sock == INVALID_SOCKET) {
		mutex_exit(&vos->vos_sock_lock);
		return;
	}

	spa_t *spa = vd->vdev_spa;
	boolean_t destroy = spa_state(spa) == POOL_STATE_DESTROYED;

	/*
	 * We need to ensure that we only issue a request when the
	 * socket is ready. Otherwise, we block here since the agent
	 * might be in recovery.
	 */
	zfs_dbgmsg("stop_agent() destroy=%d", destroy);
	zfs_object_store_wait(vos, VOS_SOCK_READY);

	// Tell agent to destroy if needed.
	agent_close_pool(vos, destroy);

	agent_wait_serial(vos, VOS_SERIAL_CLOSE_POOL);
}

static int
agent_resume_state_check(vdev_t *vd)
{
	vdev_object_store_t *vos = vd->vdev_tsd;

	/*
	 * If we're resuming in the middle of pool creation,
	 * then the agent may not have any on-disk state yet.
	 * We wait till after TXG_INITIAL to ensure that
	 * the agent has fully processed our initial transaction
	 * group.
	 */
	if (vd->vdev_spa->spa_load_state == SPA_LOAD_CREATE &&
	    vd->vdev_spa->spa_uberblock.ub_txg <= TXG_INITIAL) {
		return (0);
	}

	if (memcmp(&vd->vdev_spa->spa_ubsync, &vos->vos_uberblock,
	    sizeof (uberblock_t)) == 0) {
		return (0);
	}
	if (vos->vos_send_txg_selector == VOS_TXG_END) {
		/*
		 * In this case, it's possible that the uberblock was written
		 * out before we got the end txg done message. We can safely
		 * continue by sending the "end txg" command again, without
		 * doing "resume txg".
		 */
		if (memcmp(&vd->vdev_spa->spa_uberblock, &vos->vos_uberblock,
		    sizeof (uberblock_t)) == 0) {
			zfs_dbgmsg("resume: uberblock matches spa_uberblock; "
			    "calling TXG_END again");
			vos->vos_send_txg_selector = VOS_TXG_END_AGAIN;
			return (0);
		}
	}
	return (SET_ERROR(EBUSY));
}

static void
agent_resume_set_state(vdev_object_store_t *vos, agent_resume_state_t state)
{
	ASSERT(MUTEX_NOT_HELD(&vos->vos_resume_lock));

	mutex_enter(&vos->vos_resume_lock);
	vos->vos_resume_state = state;
	mutex_exit(&vos->vos_resume_lock);
}

static int
agent_resume_reissue(vdev_object_store_t *vos, vdev_t *vd)
{
	agent_resume_set_state(vos, VOS_RESUME_REISSUE);

	mutex_enter(&vos->vos_sock_lock);

	if (vos->vos_feature_enable != NULL) {
		agent_set_feature(vos, vos->vos_feature_enable);
	}

	vdev_queue_t *vq = &vd->vdev_queue;
	mutex_enter(&vq->vq_lock);

	for (zio_t *zio = avl_first(&vq->vq_active_tree); zio != NULL;
	    zio = AVL_NEXT(&vq->vq_active_tree, zio)) {
		uint64_t blockid = zio->io_offset >> SPA_MINBLOCKSHIFT;

		/*
		 * If we're at END state then we shouldn't have
		 * any outstanding writes in the queue.
		 */
		if (vos->vos_send_txg_selector == VOS_TXG_END) {
			VERIFY3U(zio->io_type, !=, ZIO_TYPE_WRITE);
		}

		zfs_dbgmsg("ZIO REISSUE (%px) blockid %llu",
		    zio, (u_longlong_t)blockid);

		int ret = agent_request_zio(vos, zio);
		if (ret != 0) {
			zfs_dbgmsg("agent_resume failed: %d", ret);
			mutex_exit(&vq->vq_lock);
			mutex_exit(&vos->vos_sock_lock);
			agent_resume_set_state(vos, VOS_RESUME_FAILED);
			return (-1);
		}
	}
	mutex_exit(&vq->vq_lock);

	/*
	 * process any pending stat callers
	 */
	mutex_enter(&vos->vos_stats_lock);
	for (object_store_stats_call_t *caller =
	    avl_first(&vos->vos_pending_stats_tree); caller != NULL;
	    caller = AVL_NEXT(&vos->vos_pending_stats_tree, caller)) {
		nvlist_t *request = fnvlist_alloc();

		fnvlist_add_string(request, AGENT_REQUEST_TYPE,
		    AGENT_TYPE_GET_STATS);
		fnvlist_add_uint64(request, AGENT_TOKEN, caller->oss_owner);

		zfs_dbgmsg("reissue ovdev_object_store_stats_generate, owner "
		    "0x%llx", (u_longlong_t)caller->oss_owner);

		agent_request_nv(vos, request, FTAG);
		fnvlist_free(request);
	}
	mutex_exit(&vos->vos_stats_lock);

	if (vos->vos_send_txg_selector <= VOS_TXG_END) {
		agent_resume_complete(vos);
	}

	/*
	 * We only free blocks if we haven't written
	 * out the uberblock.
	 */
	if (vos->vos_send_txg_selector == VOS_TXG_END &&
	    agent_free_blocks(vos) != 0)  {
		zfs_dbgmsg("agent_resume freeing failed");
		mutex_exit(&vos->vos_sock_lock);
		agent_resume_set_state(vos, VOS_RESUME_FAILED);
		return (-1);
	}

	if (vos->vos_send_txg_selector == VOS_TXG_END ||
	    vos->vos_send_txg_selector == VOS_TXG_END_AGAIN) {
		spa_t *spa = vd->vdev_spa;
		size_t nvlen;
		char *nvbuf = fnvlist_pack(vos->vos_config, &nvlen);
		agent_end_txg(vos, spa_syncing_txg(spa),
		    &spa->spa_uberblock, sizeof (spa->spa_uberblock),
		    nvbuf, nvlen);
		fnvlist_pack_free(nvbuf, nvlen);
	}

	/*
	 * Once we've reissued all pending I/Os, mark the socket
	 * as ready for use so that normal communication can
	 * continue.
	 */
	vos->vos_sock_state = VOS_SOCK_READY;
	cv_broadcast(&vos->vos_sock_cv);
	mutex_exit(&vos->vos_sock_lock);
	return (0);
}

static void
agent_resume(void *arg)
{
	vdev_t *vd = arg;
	vdev_object_store_t *vos = vd->vdev_tsd;
	spa_t *spa = vd->vdev_spa;
	mode_t open_mode = vdev_object_store_open_mode(spa_mode(vd->vdev_spa));
	int ret;
	boolean_t destroying = spa_state(spa) == POOL_STATE_DESTROYED;

	zfs_dbgmsg("agent_resume running");

	/* This task runs until successful completion of agent_resume_reissue */
	while (!vos->vos_agent_thread_exit) {
		/* synchronize with main agent thread */
		mutex_enter(&vos->vos_resume_lock);

		vos->vos_resume_state = VOS_RESUME_STARTING;
		cv_signal(&vos->vos_resume_cv);

		while (vos->vos_resume_state != VOS_RESUME_START) {
			cv_wait(&vos->vos_resume_cv, &vos->vos_resume_lock);
		}
		mutex_exit(&vos->vos_resume_lock);

		/*
		 * Wait till the socket is opened.
		 */
		mutex_enter(&vos->vos_sock_lock);
		zfs_object_store_wait(vos, VOS_SOCK_OPEN);

		if (!vos->vos_create_completed) {
			/*
			 * Since we're resuming a pool creation, just
			 * replay the message but don't wait for the completion.
			 * The original caller of the pool creation will be
			 * woken up when the response is received.
			 */
			agent_create_pool(vd, vos);
			mutex_exit(&vos->vos_sock_lock);
			break;
		}
		mutex_exit(&vos->vos_sock_lock);

		/*
		 * If the initial vdev open was in progress when we resumed,
		 * all we need to do is restart the open. There can't be any
		 * other outstanding requests before the initial open complets.
		 */
		boolean_t in_initial_open = !vos->vos_open_completed;

		/*
		 * If we are destroying the pool or closing it, the open call is
		 * unnecessary. Bypassing for a small performance optimization.
		 */
		if (!destroying && !vos->vos_closing) {
			agent_resume_set_state(vos, VOS_RESUME_OPENING);
			uint64_t result = agent_open_pool(vd, vos, open_mode,
			    B_TRUE);
			if (result == ERESTART) {
				zfs_dbgmsg("agent_resume retry opening pool");
				agent_resume_set_state(vos, VOS_RESUME_FAILED);
				continue;
			}
			if (result != 0) {
				zfs_dbgmsg("agent_resume: pool open failed, "
				    "err %llu", (u_longlong_t)result);
				/*
				 * It's possible we're getting an
				 * I/O error because the credentials
				 * have changed. Just retry the operation
				 * when the agent restarts so the user does
				 * not have to perform any administrative
				 * operations.
				 */
				if (result == EIO || result == ENOENT)
					continue;

				/*
				 * Other errors will result in the
				 * closing of the vdev which is fine.
				 * For example, an EREMOTEIO error
				 * indicates we're racing with another
				 * import and MMP has told us to shut
				 * ourselves down.
				 */
				vdev_set_state(vd, B_FALSE,
				    VDEV_STATE_CANT_OPEN,
				    VDEV_AUX_OPEN_FAILED);
				vos->vos_agent_thread_exit = B_TRUE;
				break;
			}
			if (in_initial_open) {
				zfs_dbgmsg("agent resume during initial open");
				break;
			}
			zfs_dbgmsg("agent resume: pool opened");
			agent_resume_set_state(vos, VOS_RESUME_OPENED);
		}
		ASSERT(!in_initial_open);
		if (vos->vos_closing) {
			mutex_enter(&vos->vos_sock_lock);
			agent_close_pool(vos, destroying);
			break;
		}

		if ((ret = agent_resume_state_check(vd)) != 0) {
			zfs_dbgmsg("agent resume failed, uberblock changed");
			vdev_set_state(vd, B_FALSE, VDEV_STATE_CANT_OPEN,
			    VDEV_AUX_MODIFIED);
			vos->vos_agent_thread_exit = B_TRUE;
			break;
		}

		/*
		 * Reissue I/O that was inflight when the agent restarted.
		 * If another agent restart occurs before we complete we
		 * start over, finishing where we left off.
		 */
		if (agent_resume_reissue(vos, vd) == 0)
			break;
	}

	agent_resume_set_state(vos, VOS_RESUME_NOT_RUNNING);
	zfs_dbgmsg("agent_resume task completed");
}

static uint64_t
object_store_create_pool(vdev_t *vd)
{
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;
	mutex_enter(&vos->vos_sock_lock);
	agent_create_pool(vd, vos);
	mutex_exit(&vos->vos_sock_lock);

	agent_wait_serial(vos, VOS_SERIAL_CREATE_POOL);
	return (vos->vos_result);
}

void
object_store_begin_txg(vdev_t *vd, uint64_t txg)
{
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;
	ASSERT(vos->vos_send_txg_selector == VOS_TXG_NONE ||
	    txg > spa_freeze_txg(vd->vdev_spa));
	mutex_enter(&vos->vos_sock_lock);
	agent_begin_txg(vos, txg);
	vos->vos_send_txg_selector = VOS_TXG_BEGIN;
	mutex_exit(&vos->vos_sock_lock);
}

static void
remove_cred_profile(nvlist_t *config)
{
	nvlist_t *tree;
	char *profile;

	tree = fnvlist_lookup_nvlist(config, ZPOOL_CONFIG_VDEV_TREE);
	if (nvlist_lookup_string(tree,
	    ZPOOL_CONFIG_CRED_PROFILE, &profile) == 0) {
		(void) nvlist_remove_all(tree, ZPOOL_CONFIG_CRED_PROFILE);
	}
}

void
object_store_end_txg(vdev_t *vd, nvlist_t *config, uint64_t txg)
{
	spa_t *spa = vd->vdev_spa;
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;
	mutex_enter(&vos->vos_sock_lock);
	/*
	 * We need to ensure that we only issue a request when the
	 * socket is ready. Otherwise, we block here since the agent
	 * might be in recovery.
	 */
	zfs_object_store_wait(vos, VOS_SOCK_READY);

	// The credentials profile should not be persisted on-disk.
	remove_cred_profile(config);

	vos->vos_send_txg_selector = VOS_TXG_END;
	if (agent_free_blocks(vos) == 0)  {
		size_t nvlen;
		char *nvbuf = fnvlist_pack(config, &nvlen);
		agent_end_txg(vos, txg,
		    &spa->spa_uberblock, sizeof (spa->spa_uberblock),
		    nvbuf, nvlen);
		fnvlist_pack_free(nvbuf, nvlen);

		if (vos->vos_config != NULL)
			fnvlist_free(vos->vos_config);
		vos->vos_config = fnvlist_dup(config);
	}

	mutex_exit(&vos->vos_sock_lock);
	agent_wait_serial(vos, VOS_SERIAL_END_TXG);

	object_store_free_block_t *osfb;
	uint64_t len = 0;
	while ((osfb = list_remove_head(&vos->vos_free_list)) != NULL) {
		kmem_free(osfb, sizeof (object_store_free_block_t));
		len++;
	}
	ASSERT(list_is_empty(&vos->vos_free_list));
	ASSERT3U(len, ==, vos->vos_free_list_len);
	vos->vos_free_list_len = 0;
	vos->vos_send_txg_selector = VOS_TXG_NONE;
}

void
object_store_free_block(vdev_t *vd, uint64_t offset, uint64_t asize)
{
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;

	/*
	 * We add freed blocks to our list which will get processed
	 * at the end of the txg.
	 */
	object_store_free_block_t *osfb =
	    kmem_alloc(sizeof (object_store_free_block_t),
	    KM_SLEEP);
	osfb->osfb_offset = offset;
	osfb->osfb_size = asize;
	list_insert_tail(&vos->vos_free_list, osfb);
	vos->vos_free_list_len++;
}

/*
 * This function will flush all the writes which have been allocated
 * as part of the mega zio regardless of where they are in the zio
 * pipeline.
 */
void
object_store_flush_all_writes(zio_t *zio)
{
	vdev_t *vd = vdev_find_leaf(zio->io_spa->spa_root_vdev,
	    &vdev_object_store_ops);
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;
	mutex_enter(&zio->io_lock);
	uint64_t blockid = zio->io_max_offset >> SPA_MINBLOCKSHIFT;
	mutex_exit(&zio->io_lock);
	agent_flush_writes(vos, blockid);
}

void
object_store_update_max_blockid(zio_t *zio)
{
	vdev_t *vd = vdev_find_leaf(zio->io_spa->spa_root_vdev,
	    &vdev_object_store_ops);
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;

	mutex_enter(&zio->io_lock);
	uint64_t blockid = zio->io_max_offset >> SPA_MINBLOCKSHIFT;
	mutex_exit(&zio->io_lock);

	uint64_t max_blockid;
	while (blockid >
	    (max_blockid = atomic_load_64(&vos->vos_max_blockid)) &&
	    (max_blockid != atomic_cas_64(&vos->vos_max_blockid,
	    max_blockid, blockid))) {
		continue;
	}
}

/*
 * Flush all writes which hold the SCL_ZIO config lock. Use the
 * vos_max_blockid since it tracks all writes which have been issued to
 * the agent and, subsequently, hold the SCL_ZIO lock.
 */
void
object_store_flush_locked_writes(spa_t *spa)
{
	vdev_t *vd = vdev_find_leaf(spa->spa_root_vdev,
	    &vdev_object_store_ops);
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;

	uint64_t max_blockid = atomic_load_64(&vos->vos_max_blockid);
	zfs_dbgmsg("object_store_flush_locked_write: max %llu",
	    (u_longlong_t)max_blockid);
	agent_flush_writes(vos, max_blockid);
}

void
object_store_get_stats(vdev_t *vd, vdev_object_store_stats_t *vossp)
{
	ASSERT(vdev_is_object_based(vd));
	vdev_object_store_t *vos = vd->vdev_tsd;

	mutex_enter(&vos->vos_stats_lock);
	*vossp = vos->vos_stats;
	mutex_exit(&vos->vos_stats_lock);
}

/*
 * Generate the object store specific stats as an nvlist
 */
static void
vdev_object_store_stats_generate(vdev_t *vd, nvlist_t *nv)
{
	ASSERT(vdev_is_object_based(vd) && vd->vdev_ops->vdev_op_leaf);
	vdev_object_store_t *vos = vd->vdev_tsd;
	object_store_stats_call_t stats_call;

	/*
	 * We need to ensure that we only issue a request when the
	 * socket is ready. Otherwise, we block here since the agent
	 * might be in recovery.
	 *
	 * When the socket is uninitialized or closed, it could be a failed
	 * open/import, in which case there is no need to collect stats and
	 * we may never reach the READY state.
	 */
	mutex_enter(&vos->vos_sock_lock);
	if (vos->vos_sock_state < VOS_SOCK_OPENING) {
		mutex_exit(&vos->vos_sock_lock);
		return;
	}
	zfs_object_store_wait(vos, VOS_SOCK_READY);

	stats_call.oss_nvl = NULL;
	stats_call.oss_owner = (uint64_t)curthread;
	mutex_init(&stats_call.oss_lock, NULL, MUTEX_DEFAULT, NULL);
	cv_init(&stats_call.oss_cv, NULL, CV_DEFAULT, NULL);

	mutex_enter(&vos->vos_stats_lock);
	avl_add(&vos->vos_pending_stats_tree, &stats_call);
	mutex_exit(&vos->vos_stats_lock);

	nvlist_t *request = fnvlist_alloc();
	fnvlist_add_string(request, AGENT_REQUEST_TYPE, AGENT_TYPE_GET_STATS);
	fnvlist_add_uint64(request, AGENT_TOKEN, stats_call.oss_owner);

	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
		zfs_dbgmsg("vdev_object_store_stats_generate(guid=%llu)",
		    (u_longlong_t)spa_guid(vd->vdev_spa));
	}

	agent_request_nv(vos, request, FTAG);
	mutex_exit(&vos->vos_sock_lock);
	fnvlist_free(request);

	/* Wait for response from agent */
	mutex_enter(&stats_call.oss_lock);
	while (stats_call.oss_nvl == NULL) {
		cv_wait(&stats_call.oss_cv, &stats_call.oss_lock);
	}
	mutex_exit(&stats_call.oss_lock);
	mutex_destroy(&stats_call.oss_lock);
	cv_destroy(&stats_call.oss_cv);

	fnvlist_add_nvlist(nv, ZPOOL_CONFIG_OBJECT_STORE_STATS,
	    stats_call.oss_nvl);
	nvlist_free(stats_call.oss_nvl);
}

static void
update_features(spa_t *spa, nvlist_t *nv)
{
	for (nvpair_t *elem = nvlist_next_nvpair(nv, NULL);
	    elem != NULL; elem = nvlist_next_nvpair(nv, elem)) {
		spa_feature_t feat;
		if (zfeature_lookup_guid(nvpair_name(elem), &feat))
			continue;

		spa->spa_feat_refcount_cache[feat] = fnvpair_value_uint64(elem);
	}
}

static void
agent_nvlist_response(vdev_object_store_t *vos, nvlist_t *nv)
{
	const char *type = fnvlist_lookup_string(nv, AGENT_RESPONSE_TYPE);
	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
		zfs_dbgmsg("got response from agent type=%s", type);
	}
	vos->vos_result = 0;
	if (strcmp(type, AGENT_TYPE_CREATE_POOL) == 0) {
		char *error = NULL;
		if (nvlist_lookup_string(nv, AGENT_ERR, &error) == 0) {
			zfs_dbgmsg("got %s err=%s msg=%s", type, error,
			    fnvlist_lookup_string(nv, AGENT_MESSAGE));
			vos->vos_result = SET_ERROR(EACCES);
		}
		vos->vos_create_completed = B_TRUE;
		agent_serial_done(vos, VOS_SERIAL_CREATE_POOL);
	} else if (strcmp(type, AGENT_TYPE_END_TXG) == 0) {
		mutex_enter(&vos->vos_stats_lock);
		vos->vos_stats.voss_blocks_count =
		    fnvlist_lookup_uint64(nv, "blocks_count");
		uint64_t old_blocks_bytes = vos->vos_stats.voss_blocks_bytes;
		vos->vos_stats.voss_blocks_bytes =
		    fnvlist_lookup_uint64(nv, "blocks_bytes");
		int64_t alloc_delta =
		    vos->vos_stats.voss_blocks_bytes - old_blocks_bytes;
		vos->vos_stats.voss_pending_frees_count =
		    fnvlist_lookup_uint64(nv, "pending_frees_count");
		vos->vos_stats.voss_pending_frees_bytes =
		    fnvlist_lookup_uint64(nv, "pending_frees_bytes");
		vos->vos_stats.voss_objects_count =
		    fnvlist_lookup_uint64(nv, "objects_count");
		mutex_exit(&vos->vos_stats_lock);

		metaslab_space_update(vos->vos_vdev,
		    vos->vos_vdev->vdev_spa->spa_normal_class,
		    alloc_delta, 0, 0);

		update_features(vos->vos_vdev->vdev_spa,
		    fnvlist_lookup_nvlist(nv, AGENT_FEATURES));

		agent_serial_done(vos, VOS_SERIAL_END_TXG);
	} else if (strcmp(type, AGENT_TYPE_OPEN_POOL) == 0) {
		char *error = NULL;
		if (nvlist_lookup_string(nv, AGENT_ERR, &error) == 0) {
			spa_t *spa = vos->vos_vdev->vdev_spa;
			zfs_dbgmsg("got %s err=%s", type, error);
			if (strcmp(error, "Mmp") == 0) {
				fnvlist_add_string(spa->spa_load_info,
				    ZPOOL_CONFIG_MMP_HOSTNAME,
				    fnvlist_lookup_string(nv, "hostname"));
				fnvlist_add_uint64(spa->spa_load_info,
				    ZPOOL_CONFIG_MMP_STATE, MMP_STATE_ACTIVE);
				fnvlist_add_uint64(spa->spa_load_info,
				    ZPOOL_CONFIG_MMP_TXG, 0);
				vos->vos_result = SET_ERROR(EREMOTEIO);
			} else if (strcmp(error, "Io") == 0) {
				char *message = fnvlist_lookup_string(nv,
				    "message");
				zfs_dbgmsg("message=\"%s\"", message);
				if (strstr(message, "does not exist") != NULL) {
					vos->vos_result = SET_ERROR(ENOENT);
				} else {
					vos->vos_result = SET_ERROR(EIO);
				}
			} else if (strcmp(error, "Checkpoint") == 0) {
				zfs_dbgmsg("Failed to find checkpoint when "
				    "attempting to rewind pool");
				vos->vos_result =
				    SET_ERROR(ZFS_ERR_NO_CHECKPOINT);
			} else {
				ASSERT0(strcmp(error, "Feature"));
				fnvlist_add_nvlist(spa->spa_load_info,
				    ZPOOL_CONFIG_UNSUP_FEAT,
				    fnvlist_lookup_nvlist(nv,
				    "invalid_features"));
				if (fnvlist_lookup_boolean_value(nv,
				    "can_readonly")) {
					fnvlist_add_boolean(spa->spa_load_info,
					    ZPOOL_CONFIG_CAN_RDONLY);
				}
				vos->vos_result = SET_ERROR(ENOTSUP);
			}
		} else {
			uint_t len;
			uint8_t *arr;
			int err = nvlist_lookup_uint8_array(nv,
			    AGENT_UBERBLOCK, &arr, &len);
			if (err == 0) {
				ASSERT3U(len, <=, sizeof (uberblock_t));
				memcpy(&vos->vos_uberblock, arr, len);

				/*
				 * We may be opening an uberblock from a pool
				 * with an older on-disk format. To handle
				 * this, we just zero out any uberblock members
				 * that did not exist when the uberblock was
				 * written.
				 */
				if (len < sizeof (uberblock_t)) {
					memset(&vos->vos_uberblock + len,
					    0, sizeof (uberblock_t) - len);
				}
				VERIFY0(nvlist_lookup_uint8_array(nv,
				    AGENT_CONFIG, &arr, &len));
				vos->vos_config = fnvlist_unpack((char *)arr,
				    len);

				update_features(vos->vos_vdev->vdev_spa,
				    fnvlist_lookup_nvlist(nv, AGENT_FEATURES));
			}

			uint64_t next_block = fnvlist_lookup_uint64(nv,
			    AGENT_NEXT_BLOCK);
			vos->vos_next_block = next_block;

			zfs_dbgmsg("got pool open done len=%u block=%llu",
			    len, (u_longlong_t)next_block);
		}
		vos->vos_open_completed = B_TRUE;
		agent_serial_done(vos, VOS_SERIAL_OPEN_POOL);
	} else if (strcmp(type, AGENT_TYPE_CLOSE_POOL) == 0) {
		zfs_dbgmsg("got %s", type);
		agent_serial_done(vos, VOS_SERIAL_CLOSE_POOL);
		mutex_enter(&vos->vos_lock);
		vos->vos_agent_thread_exit = B_TRUE;
		mutex_exit(&vos->vos_lock);
	} else if (strcmp(type, AGENT_TYPE_ENABLE_FEATURE) == 0) {
		vos->vos_feature_enable = NULL;
		agent_serial_done(vos, VOS_SERIAL_ENABLE_FEATURE);
	} else if (strcmp(type, AGENT_TYPE_GET_STATS) == 0) {
		object_store_stats_call_t *caller, search;

		nvlist_t *stats = fnvlist_lookup_nvlist(nv, AGENT_STATS);
		search.oss_owner = fnvlist_lookup_uint64(nv, AGENT_TOKEN);
		mutex_enter(&vos->vos_stats_lock);
		caller = avl_find(&vos->vos_pending_stats_tree, &search, NULL);
		if (caller != NULL)
			avl_remove(&vos->vos_pending_stats_tree, caller);
		mutex_exit(&vos->vos_stats_lock);

		if (caller != NULL) {
			mutex_enter(&caller->oss_lock);
			if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
				zfs_dbgmsg("got get stats done token 0x%llx",
				    (longlong_t)caller->oss_owner);
			}
			ASSERT(caller->oss_nvl == NULL);
			caller->oss_nvl = fnvlist_dup(stats);
			cv_broadcast(&caller->oss_cv);
			mutex_exit(&caller->oss_lock);
		} else {
			/* unexpected */
			zfs_dbgmsg("unexpected get stats done response: "
			    "owner 0x%llx", (longlong_t)search.oss_owner);
		}
	} else {
		zfs_dbgmsg("unrecognized response type '%s'!", type);
	}
}

static int
agent_reader(void *arg)
{
	vdev_object_store_t *vos = arg;

	message_header_t header;
	int err = agent_read_all(vos, &header, sizeof (header));
	if (err != 0) {
		zfs_dbgmsg("agent_reader(%px) got err %d", curthread, err);
		return (err);
	}

	union {
		read_block_response_t read_block;
		write_block_response_t write_block;
	} struct_buf;
	switch (header.message_type) {
	case MESSAGE_NVLIST:
		VERIFY0(header.struct_len);
		break;
	case MESSAGE_READ_BLOCK:
		VERIFY3U(header.struct_len, ==,
		    sizeof (struct_buf.read_block));
		err = agent_read_all(vos,
		    &struct_buf.read_block, sizeof (struct_buf.read_block));
		break;
	case MESSAGE_WRITE_BLOCK:
		VERIFY3U(header.struct_len, ==,
		    sizeof (struct_buf.write_block));
		err = agent_read_all(vos,
		    &struct_buf.write_block, sizeof (struct_buf.write_block));
		break;
	default:
		panic("invalid message_type %x", header.message_type);
	}
	if (err != 0) {
		zfs_dbgmsg("2 agent_reader(%px) got err %d",
		    curthread, err);
		return (err);
	}

	void *payload_buf = NULL;
	if (header.payload_len != 0) {
		VERIFY3U(header.payload_len, <=, vdev_object_store_max_payload);
		payload_buf = vmem_alloc(header.payload_len, KM_SLEEP);
		err = agent_read_all(vos, payload_buf, header.payload_len);
		if (err != 0) {
			zfs_dbgmsg("2 agent_reader(%px) got err %d",
			    curthread, err);
			vmem_free(payload_buf, header.payload_len);
			return (err);
		}
	}

	switch (header.message_type) {
	case MESSAGE_NVLIST: {
		nvlist_t *nv;
		err = nvlist_unpack(payload_buf, header.payload_len,
		    &nv, KM_SLEEP);
		vmem_free(payload_buf, header.payload_len);
		if (err != 0) {
			zfs_dbgmsg("got error %d from nvlist_unpack(len=%d)",
			    err, (int)header.payload_len);
			return (EAGAIN);
		}
		agent_nvlist_response(vos, nv);
		fnvlist_free(nv);
		break;
	}
	case MESSAGE_READ_BLOCK: {
		read_block_response_t *response = &struct_buf.read_block;
		if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
			zfs_dbgmsg("got read done blockid=%llu datalen=%u, "
			    "token %px",
			    (u_longlong_t)response->block,
			    header.payload_len,
			    (zio_t *)response->token);
		}
		zio_t *zio = agent_complete_zio(vos, response->token);
		VERIFY3U(response->block, ==,
		    zio->io_offset >> SPA_MINBLOCKSHIFT);
		VERIFY3U(header.payload_len, ==, zio->io_size);
		VERIFY3U(header.payload_len, ==, abd_get_size(zio->io_abd));
		abd_copy_from_buf(zio->io_abd, payload_buf, header.payload_len);
		vmem_free(payload_buf, header.payload_len);
		zio_delay_interrupt(zio);
		break;
	}
	case MESSAGE_WRITE_BLOCK: {
		write_block_response_t *response = &struct_buf.write_block;
		if (zfs_flags & ZFS_DEBUG_OBJECT_STORE) {
			zfs_dbgmsg("got write done blockid=%llu, token %px",
			    (u_longlong_t)response->block,
			    (zio_t *)response->token);
		}
		VERIFY0(header.payload_len);
		VERIFY3P(payload_buf, ==, NULL);
		zio_t *zio = agent_complete_zio(vos, response->token);
		VERIFY3U(response->block, ==,
		    zio->io_offset >> SPA_MINBLOCKSHIFT);
		zio_delay_interrupt(zio);
		break;
	}
	default:
		panic("invalid message_type %x", header.message_type);
	}

	return (0);
}

static int
vdev_object_store_socket_open(vdev_t *vd)
{
	vdev_object_store_t *vos = vd->vdev_tsd;

	while (!vos->vos_agent_thread_exit &&
	    vos->vos_sock == INVALID_SOCKET) {

		VERIFY3P(vos->vos_sock, ==, INVALID_SOCKET);

		int error = zfs_object_store_open(vos);
		if (error != 0) {
			zfs_object_store_close(vos);
			return (SET_ERROR(error));
		}

		if (vos->vos_sock == INVALID_SOCKET) {
			delay(hz);
		}
	}

	mutex_enter(&vos->vos_sock_lock);
	nvlist_t *request = fnvlist_alloc();
	fnvlist_add_string(request, AGENT_REQUEST_TYPE, AGENT_TYPE_VERSION);

	/*
	 * This specifies that the kernel supports all 1.X.Y versions of the
	 * agent communication protocol. This should be updated as new
	 * capabilities are added and supported or required.
	 */
	fnvlist_add_string(request, AGENT_VERSION, "^1");
	int rc = agent_request_nv(vos, request, FTAG);
	fnvlist_free(request);

	if (rc != 0) {
		zfs_dbgmsg("zfs_object_store_open failed to request version: "
		    "%d", rc);
		zfs_object_store_close(vos);
		mutex_exit(&vos->vos_sock_lock);
		return (SET_ERROR(EINTR));
	}

	nvlist_t *response;
	rc = agent_read_nvlist(vos, &response);
	if (rc != 0) {
		zfs_dbgmsg("zfs_object_store_open failed to receive version "
		    "negotiation response: %d", rc);
		zfs_object_store_close(vos);
		mutex_exit(&vos->vos_sock_lock);
		return (SET_ERROR(ENOTSUP));
	}
	char *type = NULL;
	rc = nvlist_lookup_string(response, AGENT_RESPONSE_TYPE, &type);
	if (rc != 0 || strcmp(type, AGENT_TYPE_VERSION) != 0) {
		zfs_dbgmsg("zfs_object_store_open received unexpected message "
		    "during negotiation: %d \"%s\"", rc,
		    type == NULL ? "" : type);
		fnvlist_free(response);
		zfs_object_store_close(vos);
		mutex_exit(&vos->vos_sock_lock);
		return (SET_ERROR(ENOTSUP));
	}
	nvlist_t *version;
	rc = nvlist_lookup_nvlist(response, AGENT_VERSION, &version);
	if (rc != 0) {
		zfs_dbgmsg("zfs_object_store_open did not receive version "
		    "during negotiation: %d", rc);
		fnvlist_free(response);
		zfs_object_store_close(vos);
		mutex_exit(&vos->vos_sock_lock);
		return (ENOTSUP);
	}
	vos->vos_version_major = fnvlist_lookup_uint64(version, "major");
	vos->vos_version_minor = fnvlist_lookup_uint64(version, "minor");
	vos->vos_version_patch = fnvlist_lookup_uint64(version, "patch");
	zfs_dbgmsg("zfs_object_store_open: Selected %llu.%llu.%llu in "
	    "negotiation", (u_longlong_t)vos->vos_version_major,
	    (u_longlong_t)vos->vos_version_minor,
	    (u_longlong_t)vos->vos_version_patch);
	fnvlist_free(response);
	if (vos->vos_version_major == 1 && vos->vos_version_minor == 0 &&
	    strcmp(vos->vos_protocol, "s3") != 0) {
		zfs_dbgmsg("zfs_object_store_open received version too low for "
		    "non-s3 protocols");
		zfs_object_store_close(vos);
		mutex_exit(&vos->vos_sock_lock);
		return (ENOTSUP);
	}
	if (zfs_flags & ZFS_DEBUG_OBJECT_STORE_SOCKET) {
		zfs_dbgmsg("SOCKET OPEN(%px): " SOCK_FMT, curthread,
		    vos->vos_sock);
	}
	vos->vos_sock_state = VOS_SOCK_OPEN;
	cv_broadcast(&vos->vos_sock_cv);
	mutex_exit(&vos->vos_sock_lock);
	return (0);
}

static void
vdev_agent_thread(void *arg)
{
	vdev_t *vd = arg;
	vdev_object_store_t *vos = vd->vdev_tsd;

	while (!vos->vos_agent_thread_exit) {

		int err = agent_reader(vos);
		if (vos->vos_agent_thread_exit || err == 0)
			continue;

		/*
		 * The agent has crashed so we need to start recovery.
		 * We first need to shutdown the socket. Manipulating
		 * the socket requires consumers to hold the vosr_sock_lock
		 * which also protects the vosr_sock_state.
		 *
		 * Once the socket is shutdown, no other thread should
		 * be able to send or receive on that socket. We also need
		 * to wakeup any threads that are currently waiting for a
		 * serial request.
		 */

		zfs_dbgmsg("(%px) agent_reader exited, reopen, err %d",
		    curthread, err);

		mutex_enter(&vos->vos_sock_lock);
		zfs_object_store_shutdown(vos);
		VERIFY3U(vos->vos_sock_state, <=, VOS_SOCK_SHUTDOWN);

		/*
		 * XXX - it's possible that the socket may reopen
		 * immediately because the connection is not completely
		 * closed by the server. To prevent this, we delay here.
		 */
		delay(hz);

		zfs_object_store_close(vos);
		mutex_exit(&vos->vos_sock_lock);
		ASSERT3P(vos->vos_sock, ==, INVALID_SOCKET);
		VERIFY3U(vos->vos_sock_state, ==, VOS_SOCK_CLOSED);

		/*
		 * Since we've successfully opened the socket before, we
		 * ignore any open errors and keep retrying.
		 */
		while ((err = vdev_object_store_socket_open(vd)) != 0) {
			zfs_dbgmsg("REOPENED(%px) sock failed " SOCK_FMT
			    ", err %d", curthread, vos->vos_sock, err);
			delay(hz);
		}
		zfs_dbgmsg("REOPENED(%px) sock " SOCK_FMT, curthread,
		    vos->vos_sock);

		/*
		 * A resume task reissues I/O that was interrupted by an
		 * agent restart. If an existing task exists help it along.
		 */
		mutex_enter(&vos->vos_resume_lock);
		if (vos->vos_resume_state != VOS_RESUME_NOT_RUNNING) {
			zfs_dbgmsg("Existing resume task running, state %d",
			    (int)vos->vos_resume_state);

			if (vos->vos_resume_state == VOS_RESUME_OPENING) {
				/* force serial waiter to give up on open */
				vos->vos_result = SET_ERROR(ERESTART);
				agent_serial_done(vos, VOS_SERIAL_OPEN_POOL);
				/* resume task waits for VOS_RESUME_START */

			}
		} else {
			VERIFY3U(taskq_dispatch(resume_taskq, agent_resume,
			    vd, TQ_SLEEP), !=, TASKQID_INVALID);
		}

		/*
		 * We need to synchronize with the agent to make sure
		 * that it's ready to receive our start signal. If the
		 * agent_resume taskq is not ready then we wait till
		 * we get signaled.
		 */
		while (vos->vos_resume_state != VOS_RESUME_STARTING) {
			cv_wait(&vos->vos_resume_cv, &vos->vos_resume_lock);
		}
		/* Signal resume task to start (avoids race for open state) */
		vos->vos_resume_state = VOS_RESUME_START;
		cv_signal(&vos->vos_resume_cv);
		mutex_exit(&vos->vos_resume_lock);
	}

	mutex_enter(&vos->vos_lock);
	vos->vos_agent_thread = NULL;
	cv_broadcast(&vos->vos_cv);
	mutex_exit(&vos->vos_lock);
	zfs_dbgmsg("agent thread exited");
	thread_exit();
}

static int
pending_stats_compare(const void *x1, const void *x2)
{
	return (TREE_CMP(((const object_store_stats_call_t *)x1)->oss_owner,
	    ((const object_store_stats_call_t *)x2)->oss_owner));
}

static int
vdev_object_store_init(spa_t *spa, nvlist_t *nv, void **tsd)
{
	(void) spa;
	vdev_object_store_t *vos;
	char *val = NULL;

	if (resume_taskq == NULL) {
		taskq_t *tq = taskq_create("agent_resume", 1, defclsyspri, 1,
		    INT_MAX, 0);
		// Only allow one taskq allocation to succeed.
		if (atomic_cas_ptr(&resume_taskq, NULL, tq) != NULL) {
			taskq_destroy(tq);
		}
	}

	vos = *tsd = kmem_zalloc(sizeof (vdev_object_store_t), KM_SLEEP);
	vos->vos_sock = INVALID_SOCKET;
	vos->vos_vdev = NULL;
	vos->vos_send_txg_selector = VOS_TXG_NONE;
	mutex_init(&vos->vos_lock, NULL, MUTEX_DEFAULT, NULL);
	mutex_init(&vos->vos_stats_lock, NULL, MUTEX_DEFAULT, NULL);
	mutex_init(&vos->vos_sock_lock, NULL, MUTEX_DEFAULT, NULL);
	mutex_init(&vos->vos_resume_lock, NULL, MUTEX_DEFAULT, NULL);
	mutex_init(&vos->vos_outstanding_lock, NULL, MUTEX_DEFAULT, NULL);
	rw_init(&vos->vos_sock_rwlock, NULL, RW_DEFAULT, NULL);
	cv_init(&vos->vos_cv, NULL, CV_DEFAULT, NULL);
	cv_init(&vos->vos_sock_cv, NULL, CV_DEFAULT, NULL);
	cv_init(&vos->vos_resume_cv, NULL, CV_DEFAULT, NULL);
	cv_init(&vos->vos_outstanding_cv, NULL, CV_DEFAULT, NULL);
	avl_create(&vos->vos_pending_stats_tree, pending_stats_compare,
	    sizeof (object_store_stats_call_t),
	    offsetof(object_store_stats_call_t, oss_node));

	list_create(&vos->vos_free_list, sizeof (object_store_free_block_t),
	    offsetof(object_store_free_block_t, osfb_list_node));

	if (!nvlist_lookup_string(nv,
	    zpool_prop_to_name(ZPOOL_PROP_OBJ_PROTOCOL), &val)) {
		vos->vos_protocol = kmem_strdup(val);
	} else {
		vos->vos_protocol = kmem_strdup("s3");
	}
	if (!nvlist_lookup_string(nv,
	    zpool_prop_to_name(ZPOOL_PROP_OBJ_ENDPOINT), &val)) {
		vos->vos_endpoint = kmem_strdup(val);
	} else if (strcmp(vos->vos_protocol, "s3") == 0) {
		return (SET_ERROR(EINVAL));
	}
	if (!nvlist_lookup_string(nv,
	    zpool_prop_to_name(ZPOOL_PROP_OBJ_REGION), &val)) {
		vos->vos_region = kmem_strdup(val);
	} else if (strcmp(vos->vos_protocol, "s3") == 0) {
		return (SET_ERROR(EINVAL));
	}
	if (!nvlist_lookup_string(nv, ZPOOL_CONFIG_CRED_PROFILE, &val)) {
		vos->vos_cred_profile = kmem_strdup(val);
	}

	zfs_dbgmsg("vdev_object_store_init, protocol=%s endpoint=%s region=%s "
	    "profile=%s", vos->vos_protocol, vos->vos_endpoint, vos->vos_region,
	    vos->vos_cred_profile);

	return (0);
}

static void
vdev_object_store_fini(vdev_t *vd)
{
	vdev_object_store_t *vos = vd->vdev_tsd;

	ASSERT(list_is_empty(&vos->vos_free_list));
	list_destroy(&vos->vos_free_list);
	mutex_destroy(&vos->vos_lock);
	mutex_destroy(&vos->vos_stats_lock);
	mutex_destroy(&vos->vos_sock_lock);
	mutex_destroy(&vos->vos_resume_lock);
	mutex_destroy(&vos->vos_outstanding_lock);
	rw_destroy(&vos->vos_sock_rwlock);
	cv_destroy(&vos->vos_cv);
	cv_destroy(&vos->vos_sock_cv);
	cv_destroy(&vos->vos_resume_cv);
	cv_destroy(&vos->vos_outstanding_cv);
	avl_destroy(&vos->vos_pending_stats_tree);
	if (vos->vos_protocol != NULL) {
		kmem_strfree(vos->vos_protocol);
	}
	if (vos->vos_endpoint != NULL) {
		kmem_strfree(vos->vos_endpoint);
	}
	if (vos->vos_region != NULL) {
		kmem_strfree(vos->vos_region);
	}
	if (vos->vos_cred_profile != NULL) {
		kmem_strfree(vos->vos_cred_profile);
	}
	if (vos->vos_config != NULL) {
		fnvlist_free(vos->vos_config);
	}
	kmem_free(vd->vdev_tsd, sizeof (vdev_object_store_t));
	vd->vdev_tsd = NULL;

	zfs_dbgmsg("vdev_object_store_fini");
}

static int
vdev_object_store_open(vdev_t *vd, uint64_t *psize, uint64_t *max_psize,
    uint64_t *logical_ashift, uint64_t *physical_ashift)
{
	int error = 0;

	/*
	 * Rotational optimizations only make sense on block devices.
	 */
	vd->vdev_nonrot = B_TRUE;

	/*
	 * Allow TRIM on object store based vdevs.  This may not always
	 * be supported, since it depends on your kernel version and
	 * underlying filesystem type but it is always safe to attempt.
	 */
	vd->vdev_has_trim = B_FALSE;

	/*
	 * Disable secure TRIM on object store based vdevs.
	 */
	vd->vdev_has_securetrim = B_FALSE;

	/*
	 * We use the pathname to specfiy the object store name.
	 */
	if (vd->vdev_path == NULL) {
		vd->vdev_stat.vs_aux = VDEV_AUX_BAD_LABEL;
		return (SET_ERROR(EINVAL));
	}

	/*
	 * Reopen the device if it's not currently open.  Otherwise,
	 * just update the physical size of the device.
	 */
	if (vd->vdev_reopening) {
		goto skip_open;
	}

	/*
	 * At this point, we can initialize aspects of the vdev
	 * which must not change as part of a vdev_reopen.
	 */
	vdev_object_store_t *vos = vd->vdev_tsd;
	vos->vos_vdev = vd;
	vos->vos_open_completed = B_FALSE;
	vos->vos_closing = B_FALSE;
	vos->vos_max_blockid = 0;

	ASSERT(vd->vdev_path != NULL);
	ASSERT3P(vos->vos_agent_thread, ==, NULL);

	error = vdev_object_store_socket_open(vd);
	if (error) {
		vd->vdev_stat.vs_aux = VDEV_AUX_OPEN_FAILED;
		return (error);
	}

	vos->vos_agent_thread = thread_create(NULL, 0, vdev_agent_thread,
	    vd, 0, &p0, TS_RUN, defclsyspri);

	if (vd->vdev_spa->spa_load_state == SPA_LOAD_CREATE) {
		vos->vos_create_completed = B_FALSE;
		error = object_store_create_pool(vd);
		if (error != 0) {
			zfs_dbgmsg("agent_create_pool failed with %d", error);
		}
	} else {
		vos->vos_create_completed = B_TRUE;
	}

	if (error == 0) {
		error = agent_open_pool(vd, vos,
		    vdev_object_store_open_mode(spa_mode(vd->vdev_spa)),
		    B_FALSE);
	}

	/*
	 * Socket is now ready for communication, wake up
	 * anyone waiting.
	 */
	mutex_enter(&vos->vos_sock_lock);
	vos->vos_sock_state = VOS_SOCK_READY;
	cv_broadcast(&vos->vos_sock_cv);
	mutex_exit(&vos->vos_sock_lock);

skip_open:

	/*
	 * XXX - We can only support ~1EB since the metaslab weights
	 * use some of the high order bits.
	 */
	if (!error) {
		*max_psize = *psize = (1ULL << 60) - 1;
		*logical_ashift = vdev_object_store_logical_ashift;
		*physical_ashift = vdev_object_store_physical_ashift;
	}

	return (error);
}

static void
vdev_object_store_close(vdev_t *vd)
{
	vdev_object_store_t *vos = vd->vdev_tsd;

	if (vd->vdev_reopening || vos == NULL)
		return;

	object_store_stop_agent(vd);

	mutex_enter(&vos->vos_lock);
	vos->vos_agent_thread_exit = B_TRUE;
	vos->vos_vdev = NULL;

	mutex_enter(&vos->vos_sock_lock);
	zfs_object_store_shutdown(vos);
	mutex_exit(&vos->vos_sock_lock);

	while (vos->vos_agent_thread != NULL) {
		zfs_dbgmsg("vdev_object_store_close: shutting down agent");
		cv_wait(&vos->vos_cv, &vos->vos_lock);
	}

	mutex_enter(&vos->vos_sock_lock);
	zfs_object_store_close(vos);
	mutex_exit(&vos->vos_sock_lock);

	mutex_exit(&vos->vos_lock);
	ASSERT3P(vos->vos_sock, ==, INVALID_SOCKET);
	vd->vdev_delayed_close = B_FALSE;
}

static void
vdev_object_store_io_start(zio_t *zio)
{
	vdev_t *vd = zio->io_vd;
	vdev_object_store_t *vos = vd->vdev_tsd;

	if (zio->io_type == ZIO_TYPE_IOCTL) {
		/* XXPOLICY */
		if (!vdev_readable(vd)) {
			zio->io_error = SET_ERROR(ENXIO);
			zio_interrupt(zio);
			return;
		}

		switch (zio->io_cmd) {
		case DKIOCFLUSHWRITECACHE:

			if (zfs_nocacheflush)
				break;

			/*
			 * XXX - may need a new ioctl sinc this will
			 * sync the entire object store.
			 */
			break;
		default:
			zio->io_error = SET_ERROR(ENOTSUP);
		}

		zio_execute(zio);
		return;
	} else if (zio->io_type == ZIO_TYPE_TRIM) {
		/* XXX - Don't support it right now */
		zio->io_error = SET_ERROR(ENOTSUP);
		zio_execute(zio);
		return;
	}

	/*
	 * We need to ensure that we only issue a request when the
	 * socket is ready. Otherwise, we block here since the agent
	 * might be in recovery.
	 */
	mutex_enter(&vos->vos_sock_lock);
	zfs_object_store_wait(vos, VOS_SOCK_READY);

	zio->io_target_timestamp = zio_handle_io_delay(zio);

	vdev_queue_t *vq = &vd->vdev_queue;
	mutex_enter(&vq->vq_lock);
	vdev_queue_pending_add(vq, zio);
	mutex_exit(&vq->vq_lock);

	agent_request_zio(vos, zio);
	mutex_exit(&vos->vos_sock_lock);
}

static void
vdev_object_store_io_done(zio_t *zio)
{
	(void) zio;
}

static void
vdev_object_store_config_generate(vdev_t *vd, nvlist_t *nv, boolean_t getstats)
{
	vdev_object_store_t *vos = vd->vdev_tsd;

	fnvlist_add_string(nv,
	    zpool_prop_to_name(ZPOOL_PROP_OBJ_PROTOCOL), vos->vos_protocol);
	if (vos->vos_endpoint != NULL) {
		fnvlist_add_string(nv,
		    zpool_prop_to_name(ZPOOL_PROP_OBJ_ENDPOINT),
		    vos->vos_endpoint);
	}
	if (vos->vos_region != NULL) {
		fnvlist_add_string(nv,
		    zpool_prop_to_name(ZPOOL_PROP_OBJ_REGION), vos->vos_region);
	}
	if (vos->vos_cred_profile != NULL) {
		fnvlist_add_string(nv, ZPOOL_CONFIG_CRED_PROFILE,
		    vos->vos_cred_profile);
	}

	if (getstats && spa_load_state(vd->vdev_spa) == SPA_LOAD_NONE)
		vdev_object_store_stats_generate(vd, nv);
}

static void
vdev_object_store_metaslab_init(vdev_t *vd, metaslab_t *msp,
    uint64_t *ms_start, uint64_t *ms_size)
{
	(void) ms_start, (void) ms_size;
	vdev_object_store_t *vos = vd->vdev_tsd;
	msp->ms_lbas[0] = vos->vos_next_block;
}

uberblock_t *
vdev_object_store_get_uberblock(vdev_t *vd)
{
	ASSERT(vdev_is_object_based(vd) && vd->vdev_ops->vdev_op_leaf);
	vdev_object_store_t *vos = vd->vdev_tsd;
	return (&vos->vos_uberblock);
}

nvlist_t *
vdev_object_store_get_config(vdev_t *vd)
{
	vdev_object_store_t *vos = vd->vdev_tsd;
	return (fnvlist_dup(vos->vos_config));
}

static void
vdev_object_store_enable_feature(vdev_t *vd, zfeature_info_t *zfeature)
{
	vdev_object_store_t *vos = vd->vdev_tsd;
	mutex_enter(&vos->vos_sock_lock);
	zfs_object_store_wait(vos, VOS_SOCK_READY);

	agent_set_feature(vd->vdev_tsd, zfeature->fi_guid);
	agent_wait_serial(vos, VOS_SERIAL_ENABLE_FEATURE);
}

vdev_ops_t vdev_object_store_ops = {
	.vdev_op_init = vdev_object_store_init,
	.vdev_op_fini = vdev_object_store_fini,
	.vdev_op_open = vdev_object_store_open,
	.vdev_op_close = vdev_object_store_close,
	.vdev_op_asize = vdev_default_asize,
	.vdev_op_min_asize = vdev_default_min_asize,
	.vdev_op_min_alloc = NULL,
	.vdev_op_io_start = vdev_object_store_io_start,
	.vdev_op_io_done = vdev_object_store_io_done,
	.vdev_op_state_change = NULL,
	.vdev_op_need_resilver = NULL,
	.vdev_op_hold = NULL,
	.vdev_op_rele = NULL,
	.vdev_op_remap = NULL,
	.vdev_op_xlate = vdev_default_xlate,
	.vdev_op_rebuild_asize = NULL,
	.vdev_op_metaslab_init = vdev_object_store_metaslab_init,
	.vdev_op_config_generate = vdev_object_store_config_generate,
	.vdev_op_nparity = NULL,
	.vdev_op_ndisks = NULL,
	.vdev_op_enable_feature = vdev_object_store_enable_feature,
	.vdev_op_type = VDEV_TYPE_OBJSTORE,	/* name of this vdev type */
	.vdev_op_leaf = B_TRUE			/* leaf vdev */
};

ZFS_MODULE_PARAM(zfs_vdev_object_store, vdev_object_store_,
    logical_ashift, ULONG, ZMOD_RW,
	"Logical ashift for object store based devices");
ZFS_MODULE_PARAM(zfs_vdev_object_store, vdev_object_store_,
    physical_ashift, ULONG, ZMOD_RW,
	"Physical ashift for object store based devices");
ZFS_MODULE_PARAM(zfs_vdev_object_store, vdev_object_store_,
    max_payload, ULONG, ZMOD_RW,
	"maximum message payload in bytes");
