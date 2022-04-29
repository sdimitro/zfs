sbin_PROGRAMS = $(RUSTDIR)/zcache $(RUSTDIR)/zcachedb $(RUSTDIR)/zfs_object_agent $(RUSTDIR)/zfs_object_perf $(RUSTDIR)/zoa_test

common_sources = $(wildcard util/src/*.rs util/Cargo.toml zettacache/src/*.rs zettacache/src/*/*.rs zettacache/Cargo.toml zettaobject/src/*/*.rs zettaobject/src/*.rs zettaobject/Cargo.toml Cargo.lock Cargo.toml)

__RUSTDIR__zcache_SOURCES = $(wildcard zcache/src/*.rs zcache/Cargo.toml) $(common_sources)

__RUSTDIR__zcachedb_SOURCES = $(wildcard zcdb/src/*.rs zcdb/Cargo.toml) $(common_sources)

__RUSTDIR__zfs_object_agent_SOURCES = $(wildcard server/src/*.rs server/Cargo.toml) $(common_sources)

__RUSTDIR__zfs_object_perf_SOURCES = $(wildcard object_perf/src/*.rs object_perf/Cargo.toml) $(common_sources)

__RUSTDIR__zoa_test_SOURCES = $(wildcard client/src/*.rs client/Cargo.toml) $(common_sources)

__RUSTDIR__libzoa_so_SOURCES = $(wildcard zoa/src/*.rs zoa/Cargo.toml) $(common_sources)

EXTRA_DIST = $(__RUSTDIR__libzoa_so_SOURCES)

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
