# NB: GNU Automake Manual, Chapter 8.3.5: Libtool Convenience Libraries
# These nine libraries are intermediary build components.
SUBDIRS = libavl libicp libshare libspl libtpool libzstd

if BUILD_LINUX
SUBDIRS += libefi
endif

# libnvpair is installed as part of the final build product
# libzutil depends on it, so it must be compiled before libzutil
SUBDIRS += libnvpair

# libzutil depends on libefi if present
SUBDIRS += libzutil libunicode

# These four libraries, which are installed as the final build product,
# incorporate the eight convenience libraries given above.
SUBDIRS += libuutil libzfs_core libzfs libzpool

# [Serapheim]: This library is a final build product in and of itself
#              too, and placed here at the end as it may incorporate
#              libzpool. The current reasoning behind only building it
#              for Linux is because of the syscall_intercept library
#              and the possibility that Linux FUSE is different than
#              FreeBSD FUSE.
if BUILD_LINUX
SUBDIRS += libuzfs
endif
