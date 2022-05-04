sbin_PROGRAMS = $(RUSTDIR)/zcache $(RUSTDIR)/zcachedb $(RUSTDIR)/zfs_object_agent $(RUSTDIR)/zfs_object_perf $(RUSTDIR)/zoa_test

__RUSTDIR__zcache_SOURCES = $(shell ./parse_deps.sh $(RUSTDIR)/zcache.d)

__RUSTDIR__zcachedb_SOURCES = $(shell ./parse_deps.sh $(RUSTDIR)/zcachedb.d)

__RUSTDIR__zfs_object_agent_SOURCES = $(shell ./parse_deps.sh $(RUSTDIR)/zfs_object_agent.d)

__RUSTDIR__zfs_object_perf_SOURCES = $(shell ./parse_deps.sh $(RUSTDIR)/zfs_object_perf.d)

__RUSTDIR__zoa_test_SOURCES = $(shell ./parse_deps.sh $(RUSTDIR)/zoa_test.d)

__RUSTDIR__libzoa_so_SOURCES = $(shell ./parse_deps.sh $(RUSTDIR)/libzoa.d)

EXTRA_DIST = $(__RUSTDIR__libzoa_so_SOURCES) parse_deps.sh

RUSTFLAGS = \
	-L $(abs_top_builddir)/lib/libzfs/.libs \
	-L $(abs_top_builddir)/lib/libnvpair/.libs

GITREV = $(shell cat $(abs_top_builddir)/include/zfs_gitrev.h | awk '{print $$3}')

.PHONY: \
	build

$(RUSTDIR)/libzoa.so : $(__RUSTDIR__libzoa_so_SOURCES)
	CARGO_ZOA_GITREV="$(GITREV)" RUSTFLAGS="$(RUSTFLAGS)" cargo build --package zoa $(RUSTTARGET)

$(RUSTDIR)/zfs_object_agent$(EXEEXT) : $(__RUSTDIR__zfs_object_agent_SOURCES)
	CARGO_ZOA_GITREV="$(GITREV)" RUSTFLAGS="$(RUSTFLAGS)" cargo build --package zfs_object_agent $(RUSTTARGET)

$(RUSTDIR)/zfs_object_perf$(EXEEXT) : $(__RUSTDIR__zfs_object_perf_SOURCES)
	CARGO_ZOA_GITREV="$(GITREV)" RUSTFLAGS="$(RUSTFLAGS)" cargo build --package zfs_object_perf $(RUSTTARGET)

$(RUSTDIR)/zoa_test$(EXEEXT) : $(__RUSTDIR__zoa_test_SOURCES)
	CARGO_ZOA_GITREV="$(GITREV)" RUSTFLAGS="$(RUSTFLAGS)" cargo build --package zoa_test $(RUSTTARGET)

$(RUSTDIR)/zcachedb$(EXEEXT) : $(__RUSTDIR__zcachedb_SOURCES)
	CARGO_ZOA_GITREV="$(GITREV)" RUSTFLAGS="$(RUSTFLAGS)" cargo build --package zcachedb $(RUSTTARGET)

$(RUSTDIR)/zcache$(EXEEXT) : $(__RUSTDIR__zcache_SOURCES)
	CARGO_ZOA_GITREV="$(GITREV)" RUSTFLAGS="$(RUSTFLAGS)" cargo build --package zcache $(RUSTTARGET)

all: build

build: \
	$(RUSTDIR)/libzoa.so \
	$(RUSTDIR)/zcache$(EXEEXT) \
	$(RUSTDIR)/zcachedb$(EXEEXT) \
	$(RUSTDIR)/zfs_object_agent$(EXEEXT) \
	$(RUSTDIR)/zfs_object_perf$(EXEEXT) \
	$(RUSTDIR)/zoa_test$(EXEEXT)

install-exec-hook :
	cp $(RUSTDIR)/libzoa.so $(DESTDIR)$(libdir)/libzoa.so
