#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>

#include <libzfs.h>

#include <sys/zfs_ioctl.h>

// actual fd of the ioctl() doesn't matter for the time being
// as long as we get intercepted.
static int ioctl_fd = 0;

void test_invalid_ioctl(void);
void test_create_pool(void);
void test_export_pool(void);

void
test_invalid_ioctl(void)
{
	int err = ioctl(ioctl_fd, 0);
	if (err == 0) {
		printf("error: ioctl() returned 0 instead of -1\n");
		exit(EXIT_FAILURE);
	}
	if (errno != ENOTTY) {
		printf("errno: %d\n", errno);
		printf("error: expected invalid ioctl() to return ENOTTY(%d)\n", ENOTTY);
		exit(EXIT_FAILURE);
	}
}

void
test_create_pool(void)
{
	libzfs_handle_t *hdl = libzfs_init();
	if (hdl == NULL) {
		(void) fprintf(stderr, "%s\n", libzfs_error_init(errno));
		exit(EXIT_FAILURE);
	}

	nvlist_t *nvroot = NULL;
	verify(nvlist_alloc(&nvroot, NV_UNIQUE_NAME, 0) == 0);
	verify(nvlist_add_string(nvroot, ZPOOL_CONFIG_TYPE, VDEV_TYPE_ROOT) == 0);


	nvlist_t *vdev = NULL;
	verify(nvlist_alloc(&vdev, NV_UNIQUE_NAME, 0) == 0);
	verify(nvlist_add_string(vdev, ZPOOL_CONFIG_PATH, "/dev/sdb") == 0);
	verify(nvlist_add_string(vdev, ZPOOL_CONFIG_TYPE, "disk") == 0);
	verify(nvlist_add_uint64(vdev, ZPOOL_CONFIG_IS_LOG, B_FALSE) == 0);
	verify(nvlist_add_uint64(vdev, ZPOOL_CONFIG_WHOLE_DISK, (uint64_t)B_TRUE) == 0);
	verify(nvlist_add_uint64(vdev, ZPOOL_CONFIG_ASHIFT, 9) == 0);
	verify(nvlist_add_nvlist_array(nvroot, ZPOOL_CONFIG_CHILDREN, &vdev, 1) == 0);

	int err = zpool_create(hdl, "domainZ", nvroot, NULL, NULL);
	if (err != 0) {
		printf("error: zpool_create errno(%d): %d\n", errno, err);
		nvlist_free(vdev);
		nvlist_free(nvroot);
		exit(EXIT_FAILURE);
	}

	nvlist_free(vdev);
	nvlist_free(nvroot);
	libzfs_fini(hdl);
}

void
test_export_pool(void)
{
	char *history_str = calloc(1, HIS_MAX_RECORD_LEN);

	zfs_cmd_t *zc = calloc(1, sizeof(*zc));
	(void) strlcpy(zc->zc_name, "domainZ", sizeof (zc->zc_name));
	zc->zc_cookie = B_FALSE;
	zc->zc_guid = B_FALSE;
	zc->zc_history = (uint64_t)(uintptr_t)history_str;

	int err = ioctl(ioctl_fd, ZFS_IOC_POOL_EXPORT, zc);
	if (err != 0) {
		printf("error: zpool_create errno(%d): %d\n", errno, err);
		free(history_str);
		free(zc);
		exit(EXIT_FAILURE);
	}
	free(history_str);
	free(zc);
}

int
main(void)
{
	test_invalid_ioctl();
	printf("TEST_INVALID_IOCTL: SUCCESS!\n");
	test_create_pool();
	printf("TEST_CREATE_POOL: SUCCESS!\n");
	test_export_pool();
	printf("TEST_EXPORT_POOL: SUCCESS!\n");
	return 0;
}
