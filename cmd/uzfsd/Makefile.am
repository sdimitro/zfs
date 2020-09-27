include $(top_srcdir)/config/Rules.am

# Get rid of compiler warning for unchecked truncating snprintfs on gcc 7.1.1
AM_CFLAGS += $(NO_FORMAT_TRUNCATION)

# Includes kernel code, generate warnings for large stack frames
AM_CFLAGS += $(FRAME_LARGER_THAN)

# Unconditionally enable ASSERTs
AM_CPPFLAGS += -DDEBUG -UNDEBUG

sbin_PROGRAMS = uzfsd

uzfsd_SOURCES = \
	uzfsd.c

uzfsd_LDADD = \
	$(top_builddir)/lib/libnvpair/libnvpair.la \
	$(abs_top_builddir)/lib/libzfs/libzfs.la \
	$(abs_top_builddir)/lib/libzfs_core/libzfs_core.la \
	$(top_builddir)/lib/libzpool/libzpool.la

# XXX: try and see if you can link libuzfs directly and see if
# you can skip the preloading.

uzfsd_LDADD += -lm
uzfsd_LDFLAGS = -pthread
