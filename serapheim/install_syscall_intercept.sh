#!/bin/sh

git clone https://github.com/pmem/syscall_intercept.git
cd syscall_intercept

sudo apt update

# Runtime deps
sudo apt install pkg-config libcapstone3 libcapstone-dev

# Build deps
sudo apt install cmake perl pandoc

# From the repo's root
mkdir build; cd build
cmake ..
make
sudo make install

# Optionally
make test
