include $(top_srcdir)/config/Rules.am

VPATH = \
	$(top_srcdir)/module/zfs \
	$(top_srcdir)/module/zcommon \
	$(top_srcdir)/module/lua \
	$(top_srcdir)/module/os/linux/zfs \
	$(top_srcdir)/lib/libzpool

# Unconditionally enable ASSERTs
AM_CPPFLAGS += -DDEBUG -UNDEBUG

AM_CFLAGS += -DLIB_UZFS_BUILD

lib_LTLIBRARIES = libuzfs.la

USER_C = \
	uzfs.c

dist_libuzfs_la_SOURCES = \
	$(USER_C)

libuzfs_la_LIBADD = \
	$(abs_top_builddir)/lib/libzpool/libzpool.la

libuzfs_la_LIBADD += -lsyscall_intercept

if !ASAN_ENABLED
libuzfs_la_LDFLAGS = -Wl,-z,defs
endif
