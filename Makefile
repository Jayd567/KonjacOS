# Top-level build orchestration for KonjacOS.
#
# Targets:
#   make kernel   - build the Rust kernel binary only
#   make iso      - build the kernel and package a bootable ISO (image.iso)
#   make kfs      - build the KonjacFS disk (kfs.img, mounted at /) from disk_root/
#   make disk     - build the FAT16 disk (disk.img, mounted at /fat) from disk_root/
#   make run      - build the ISO + disk and boot them in QEMU (BIOS)
#   make run-uefi - build the ISO + disk and boot them in QEMU (UEFI, via OVMF)
#   make release  - build both images and package them into dist/
#   make clean    - remove build output
#
# `make disk` needs `mkfs.vfat` and `mcopy` (Debian/Ubuntu: `apt install
# dosfstools mtools`). If they're missing, `disk` is skipped with a
# warning -- the OS still boots and runs the shell fine, just without a
# filesystem to `ls`/`cat` from (it prints as much at boot).

MODE          ?= dev
KERNEL_DIR    := kernel
LIMINE_DIR    := limine
ISO_ROOT      := iso_root
IMAGE         := image.iso
DISK_ROOT     := disk_root
DISK_IMAGE    := disk.img
DISK_SIZE_MB  := 400
KFS_IMAGE     := kfs.img
KFS_SIZE      := 512M
LIMINE_VERSION := 9.6.7
VERSION       ?= $(shell git describe --tags --always 2>/dev/null || echo dev)
DIST          := dist
LIMINE_BINS   := $(LIMINE_DIR)/limine-bios.sys $(LIMINE_DIR)/limine-bios-cd.bin $(LIMINE_DIR)/limine-uefi-cd.bin

ifeq ($(MODE),release)
	CARGO_FLAGS := --release
	KERNEL_BIN  := $(KERNEL_DIR)/target/x86_64-unknown-linux-gnu/release/kernel
else
	CARGO_FLAGS :=
	KERNEL_BIN  := $(KERNEL_DIR)/target/x86_64-unknown-linux-gnu/debug/kernel
endif

# -boot order=d: always boot the CD-ROM first, regardless of whether a
# data disk is also attached -- without this, some BIOSes try the (non-
# bootable) data disk first, which can stall for several seconds or more
# before it falls through to the CD-ROM.
QEMU_FLAGS := -m 256M -serial stdio -no-reboot -no-shutdown -boot order=d -rtc base=localtime

.PHONY: all
all: iso

.PHONY: kernel
kernel:
	cd $(KERNEL_DIR) && cargo build $(CARGO_FLAGS)

# The BIOS/CD boot stages are binary-only and not committed; fetch them from
# the official Limine binary release that matches the committed limine.h.
$(LIMINE_BINS):
	rm -rf .cache/limine
	git clone --quiet --depth 1 --branch v$(LIMINE_VERSION)-binary \
		https://github.com/limine-bootloader/limine.git .cache/limine
	cp .cache/limine/limine-bios.sys .cache/limine/limine-bios-cd.bin \
		.cache/limine/limine-uefi-cd.bin $(LIMINE_DIR)/

$(LIMINE_DIR)/limine: $(LIMINE_DIR)/limine.c
	$(MAKE) -C $(LIMINE_DIR)

.PHONY: iso
iso: kernel $(LIMINE_BINS) $(LIMINE_DIR)/limine
	rm -rf $(ISO_ROOT)
	mkdir -p $(ISO_ROOT)/boot/limine
	mkdir -p $(ISO_ROOT)/EFI/BOOT
	cp $(KERNEL_BIN) $(ISO_ROOT)/boot/kernel.elf
	cp boot/limine.conf $(ISO_ROOT)/boot/limine/
	cp $(LIMINE_DIR)/limine-bios.sys $(LIMINE_DIR)/limine-bios-cd.bin $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT)/boot/limine/
	cp $(LIMINE_DIR)/BOOTX64.EFI $(ISO_ROOT)/EFI/BOOT/
	cp $(LIMINE_DIR)/BOOTIA32.EFI $(ISO_ROOT)/EFI/BOOT/
	xorriso -as mkisofs -R -r -J -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table \
		--efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		$(ISO_ROOT) -o $(IMAGE)
	./$(LIMINE_DIR)/limine bios-install $(IMAGE)
	@echo "Built $(IMAGE)"

.PHONY: disk
disk:
	@if [ -f $(DISK_IMAGE) ]; then \
		echo "$(DISK_IMAGE) already exists, leaving it alone (rm it to regenerate from $(DISK_ROOT)/)"; \
	elif ! command -v mkfs.vfat >/dev/null || ! command -v mcopy >/dev/null; then \
		echo "mkfs.vfat/mcopy not found (apt install dosfstools mtools) -- skipping $(DISK_IMAGE)."; \
		echo "The OS still boots without it, just with no filesystem to ls/cat."; \
	else \
		truncate -s $(DISK_SIZE_MB)M $(DISK_IMAGE); \
		mkfs.vfat -F 16 -n KONJACOS $(DISK_IMAGE) >/dev/null; \
		mcopy -s -i $(DISK_IMAGE) $(DISK_ROOT)/* ::; \
		echo "Built $(DISK_IMAGE) from $(DISK_ROOT)/ (including subdirectories)"; \
	fi

# The KonjacFS disk, built by tools/kfs.py (Python 3, standard library
# only): KonjacOS's main disk, mounted at /. FAT16 is then at /fat.
.PHONY: kfs
kfs:
	@if [ -f $(KFS_IMAGE) ]; then 		echo "$(KFS_IMAGE) already exists, leaving it alone (rm it to regenerate from $(DISK_ROOT)/)"; 	else 		python3 tools/kfs.py mkfs $(DISK_ROOT) $(KFS_IMAGE) --size $(KFS_SIZE); 	fi

comma := ,

# Copies files (from disk_root/) to the root of whichever disk images exist,
# leaving everything else on them alone.
define add-to-disks
	@if [ -f $(KFS_IMAGE) ]; then for f in $(1); do python3 tools/kfs.py put $(KFS_IMAGE) $$f /; done; fi
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(1) ::/; fi
endef

# virtio: the fast DMA disk driver (`virtio_blk.rs`). `if=ide` works too,
# through the slower ATA driver (FAT16 disk only).
DISK_DRIVE = $(if $(wildcard $(DISK_IMAGE)),-drive file=$(DISK_IMAGE)$(comma)format=raw$(comma)if=virtio) 	$(if $(wildcard $(KFS_IMAGE)),-drive file=$(KFS_IMAGE)$(comma)format=raw$(comma)if=virtio)

.PHONY: run
run: iso disk kfs
	qemu-system-x86_64 $(QEMU_FLAGS) -cdrom $(IMAGE) $(DISK_DRIVE)

.PHONY: run-uefi
run-uefi: iso disk kfs
	@test -f /usr/share/OVMF/OVMF_CODE_4M.fd || \
		(echo "OVMF firmware not found; install the 'ovmf' package" && exit 1)
	qemu-system-x86_64 $(QEMU_FLAGS) \
		-drive if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE_4M.fd,readonly=on \
		-drive if=pflash,format=raw,unit=1,file=/usr/share/OVMF/OVMF_VARS_4M.fd \
		-cdrom $(IMAGE) $(DISK_DRIVE)

.PHONY: release
release: iso disk kfs
	rm -rf $(DIST) && mkdir -p $(DIST)
	cp $(IMAGE) $(DIST)/konjacos-$(VERSION).iso
	cd $(DIST) && cp ../$(DISK_IMAGE) konjacos-$(VERSION)-disk.img && \
		zip -q -9 konjacos-$(VERSION)-disk.zip konjacos-$(VERSION)-disk.img && \
		rm konjacos-$(VERSION)-disk.img
	cd $(DIST) && cp ../$(KFS_IMAGE) konjacos-$(VERSION)-kfs.img && \
		zip -q -9 konjacos-$(VERSION)-kfs.zip konjacos-$(VERSION)-kfs.img && \
		rm konjacos-$(VERSION)-kfs.img
	cd $(DIST) && sha256sum * > SHA256SUMS
	@echo "Release files are in $(DIST)/"

.PHONY: clean
clean:
	cd $(KERNEL_DIR) && cargo clean
	rm -rf $(ISO_ROOT) $(IMAGE) $(DIST)
	@echo "(leaving $(DISK_IMAGE) and $(KFS_IMAGE) alone -- rm them yourself if you want fresh ones)"

# Hosted glibc fixture for the Linux ABI; not linked into the kernel.
# Preserve an existing data disk, including any JDK installed directly into it.
.PHONY: futex-fixture
futex-fixture:
	gcc -O2 -pthread userprogs/futex_glibc.c -o $(DISK_ROOT)/FUTEST.ELF
	$(call add-to-disks,$(DISK_ROOT)/FUTEST.ELF)

.PHONY: path-fixture io-fixture
path-fixture:
	gcc -O2 userprogs/realpath_glibc.c -o $(DISK_ROOT)/PATHCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/PATHCHK.ELF)

io-fixture:
	gcc -O2 userprogs/io_glibc.c -o $(DISK_ROOT)/IOCHK.ELF
	python3 -c "from pathlib import Path; Path('$(DISK_ROOT)/IOPAT.BIN').write_bytes(bytes((i*37+i//251)&255 for i in range(65539)))"
	$(call add-to-disks,$(DISK_ROOT)/IOCHK.ELF $(DISK_ROOT)/IOPAT.BIN)

.PHONY: statfs-fixture
statfs-fixture:
	gcc -O2 userprogs/statfs_glibc.c -o $(DISK_ROOT)/STATFS.ELF
	$(call add-to-disks,$(DISK_ROOT)/STATFS.ELF)

.PHONY: largefile-fixture
largefile-fixture:
	gcc -O2 userprogs/largefile_glibc.c -o $(DISK_ROOT)/BIGCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/BIGCHK.ELF)

.PHONY: vm-fixture
vm-fixture:
	gcc -O2 -pthread userprogs/vm_glibc.c -o $(DISK_ROOT)/VMCHK.ELF
	gcc -O2 userprogs/vm_fault.c -o $(DISK_ROOT)/OOMCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/VMCHK.ELF $(DISK_ROOT)/OOMCHK.ELF)

.PHONY: getcpu-fixture
getcpu-fixture:
	gcc -O2 userprogs/getcpu_glibc.c -o $(DISK_ROOT)/CPUCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/CPUCHK.ELF)

.PHONY: sysinfo-fixture
sysinfo-fixture:
	gcc -O2 userprogs/sysinfo_glibc.c -o $(DISK_ROOT)/SYSCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/SYSCHK.ELF)

.PHONY: timed-futex-fixture
timed-futex-fixture:
	gcc -O2 -pthread userprogs/futex_timed_glibc.c -o $(DISK_ROOT)/TIMECHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/TIMECHK.ELF)

.PHONY: large-read-fixture
large-read-fixture:
	gcc -O2 userprogs/read_large_glibc.c -o $(DISK_ROOT)/READCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/READCHK.ELF)

.PHONY: getcwd-fixture
getcwd-fixture:
	gcc -O2 userprogs/getcwd_glibc.c -o $(DISK_ROOT)/CWDCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/CWDCHK.ELF)

.PHONY: clone-files-fixture
clone-files-fixture:
	gcc -O2 -pthread userprogs/clone_files_glibc.c -o $(DISK_ROOT)/FDCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/FDCHK.ELF)

.PHONY: sleep-fixture
sleep-fixture:
	gcc -O2 -pthread userprogs/sleep_glibc.c -o $(DISK_ROOT)/SLEEPCHK.ELF
	$(call add-to-disks,$(DISK_ROOT)/SLEEPCHK.ELF)
