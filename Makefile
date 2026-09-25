# Top-level build orchestration for KonjacOS.
#
# Targets:
#   make kernel   - build the Rust kernel binary only
#   make iso      - build the kernel and package a bootable ISO (image.iso)
#   make disk     - build the FAT16 data disk (disk.img) from disk_root/
#   make run      - build the ISO + disk and boot them in QEMU (BIOS)
#   make run-uefi - build the ISO + disk and boot them in QEMU (UEFI, via OVMF)
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
QEMU_FLAGS := -m 256M -serial stdio -no-reboot -no-shutdown -boot order=d

.PHONY: all
all: iso

.PHONY: kernel
kernel:
	cd $(KERNEL_DIR) && cargo build $(CARGO_FLAGS)

.PHONY: iso
iso: kernel
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

comma := ,
DISK_DRIVE = $(if $(wildcard $(DISK_IMAGE)),-drive file=$(DISK_IMAGE)$(comma)format=raw$(comma)if=ide$(comma)index=0$(comma)media=disk)

.PHONY: run
run: iso disk
	qemu-system-x86_64 $(QEMU_FLAGS) -cdrom $(IMAGE) $(DISK_DRIVE)

.PHONY: run-uefi
run-uefi: iso disk
	@test -f /usr/share/OVMF/OVMF_CODE_4M.fd || \
		(echo "OVMF firmware not found; install the 'ovmf' package" && exit 1)
	qemu-system-x86_64 $(QEMU_FLAGS) \
		-drive if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE_4M.fd,readonly=on \
		-drive if=pflash,format=raw,unit=1,file=/usr/share/OVMF/OVMF_VARS_4M.fd \
		-cdrom $(IMAGE) $(DISK_DRIVE)

.PHONY: clean
clean:
	cd $(KERNEL_DIR) && cargo clean
	rm -rf $(ISO_ROOT) $(IMAGE)
	@echo "(leaving $(DISK_IMAGE) alone -- rm it yourself if you want a fresh one)"

# Hosted glibc fixture for the Linux ABI; not linked into the kernel.
# Preserve an existing data disk, including any JDK installed directly into it.
.PHONY: futex-fixture
futex-fixture:
	gcc -O2 -pthread userprogs/futex_glibc.c -o $(DISK_ROOT)/FUTEST.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/FUTEST.ELF ::/FUTEST.ELF; fi

.PHONY: path-fixture io-fixture
path-fixture:
	gcc -O2 userprogs/realpath_glibc.c -o $(DISK_ROOT)/PATHCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/PATHCHK.ELF ::/PATHCHK.ELF; fi

io-fixture:
	gcc -O2 userprogs/io_glibc.c -o $(DISK_ROOT)/IOCHK.ELF
	python3 -c "from pathlib import Path; Path('$(DISK_ROOT)/IOPAT.BIN').write_bytes(bytes((i*37+i//251)&255 for i in range(65539)))"
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/IOCHK.ELF $(DISK_ROOT)/IOPAT.BIN ::/; fi

.PHONY: largefile-fixture
largefile-fixture:
	gcc -O2 userprogs/largefile_glibc.c -o $(DISK_ROOT)/BIGCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/BIGCHK.ELF ::/BIGCHK.ELF; fi

.PHONY: vm-fixture
vm-fixture:
	gcc -O2 -pthread userprogs/vm_glibc.c -o $(DISK_ROOT)/VMCHK.ELF
	gcc -O2 userprogs/vm_fault.c -o $(DISK_ROOT)/OOMCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/VMCHK.ELF $(DISK_ROOT)/OOMCHK.ELF ::/; fi

.PHONY: getcpu-fixture
getcpu-fixture:
	gcc -O2 userprogs/getcpu_glibc.c -o $(DISK_ROOT)/CPUCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/CPUCHK.ELF ::/CPUCHK.ELF; fi

.PHONY: sysinfo-fixture
sysinfo-fixture:
	gcc -O2 userprogs/sysinfo_glibc.c -o $(DISK_ROOT)/SYSCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/SYSCHK.ELF ::/SYSCHK.ELF; fi

.PHONY: timed-futex-fixture
timed-futex-fixture:
	gcc -O2 -pthread userprogs/futex_timed_glibc.c -o $(DISK_ROOT)/TIMECHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/TIMECHK.ELF ::/TIMECHK.ELF; fi

.PHONY: large-read-fixture
large-read-fixture:
	gcc -O2 userprogs/read_large_glibc.c -o $(DISK_ROOT)/READCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/READCHK.ELF ::/READCHK.ELF; fi

.PHONY: getcwd-fixture
getcwd-fixture:
	gcc -O2 userprogs/getcwd_glibc.c -o $(DISK_ROOT)/CWDCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/CWDCHK.ELF ::/CWDCHK.ELF; fi

.PHONY: clone-files-fixture
clone-files-fixture:
	gcc -O2 -pthread userprogs/clone_files_glibc.c -o $(DISK_ROOT)/FDCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/FDCHK.ELF ::/FDCHK.ELF; fi

.PHONY: sleep-fixture
sleep-fixture:
	gcc -O2 -pthread userprogs/sleep_glibc.c -o $(DISK_ROOT)/SLEEPCHK.ELF
	@if [ -f $(DISK_IMAGE) ]; then mcopy -o -i $(DISK_IMAGE) $(DISK_ROOT)/SLEEPCHK.ELF ::/SLEEPCHK.ELF; fi
