#/bin/sh

echo "serapheim: if this fails try running as sudo before giving up."
LD_LIBRARY_PATH=/usr/local/lib/ LD_PRELOAD=/lib/libuzfs.so uzfsd
