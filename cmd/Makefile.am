SUBDIRS  = zfs zpool zdb zhack zinject zstream zstreamdump ztest
SUBDIRS += fsck_zfs vdev_id raidz_test

if USING_PYTHON
SUBDIRS += arcstat arc_summary dbufstat
endif

if BUILD_LINUX
SUBDIRS += mount_zfs zed zgenhostid zvol_id zvol_wait zfs_ids_to_path
endif
