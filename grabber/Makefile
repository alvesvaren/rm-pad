# Cross-compile for reMarkable. On the tablet, run: uname -m
#   armv7l  → reMarkable 2 (and rM1): use default or make ARCH=arm
#   aarch64 → some devices/firmware:  make ARCH=aarch64
# Native build on the tablet: make CC=gcc
#
# We use CROSS_CC (not CC) so your environment's CC=gcc doesn't build an x86 binary.
# If your toolchain has a different name, e.g. arm-linux-gnueabihf-gcc-7.5:
#   make CROSS_CC=arm-linux-gnueabihf-gcc-7.5

ARCH ?= arm
ifeq ($(ARCH),aarch64)
  CROSS_CC ?= aarch64-linux-gnu-gcc
else
  CROSS_CC ?= arm-linux-gnueabihf-gcc
endif

CFLAGS = -O2 -Wall -Wextra
TARGET = rm-mouse-grabber

all: $(TARGET)
	@file $(TARGET) | grep -q ARM || (echo "ERROR: $(TARGET) is not an ARM binary. Check CROSS_CC and ARCH."; file $(TARGET); exit 1)

$(TARGET): grabber.c
	$(CROSS_CC) $(CFLAGS) -o $@ $<

clean:
	rm -f $(TARGET)

.PHONY: all clean
