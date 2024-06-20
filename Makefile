include /usr/share/dpkg/architecture.mk
include /usr/share/dpkg/pkg-info.mk

SRCPACKAGE=rust-pxar
PACKAGE=lib$(SRCPACKAGE)-dev
ARCH := $(DEB_BUILD_ARCH)

DEB=$(PACKAGE)_$(DEB_VERSION)_$(ARCH).deb
DSC=$(SRCPACKAGE)_$(DEB_VERSION)_$(ARCH).deb

.PHONY: all
all: check

.PHONY: check
check:
	cargo test --all-features

.PHONY: dinstall
dinstall: deb
	sudo -k dpkg -i build/librust-*.deb

build:
	rm -rf build
	rm -f debian/control
	mkdir build
	debcargo package \
	    --config "$(PWD)/debian/debcargo.toml" \
	    --changelog-ready \
	    --no-overlay-write-back \
	    --directory "$(PWD)/build/pxar" \
	    "pxar" \
	    "$$(dpkg-parsechangelog -l "debian/changelog" -SVersion | sed -e 's/-.*//')"
	echo system >build/rust-toolchain
	rm -f build/pxar/Cargo.lock
	find build/pxar/debian -name '*.hint' -delete
	cp build/pxar/debian/control debian/control

.PHONY: deb
deb:
	rm -rf build
	$(MAKE) build/$(DEB)
build/$(DEB): build
	(cd build/pxar && CARGO=/usr/bin/cargo RUSTC=/usr/bin/rustc dpkg-buildpackage -b -uc -us)
	lintian build/*.deb

.PHONY: clean
clean:
	rm -rf build *.deb *.buildinfo *.changes *.orig.tar.gz
	cargo clean

.PHONY: upload
upload: UPLOAD_DIST ?= $(DEB_DISTRIBUTION)
upload: build/$(DEB)
	cd build; \
	    dcmd --deb rust-pxar_*.changes \
	    | grep -v '.changes$$' \
	    | tar -cf- -T- \
	    | echo ssh -X repoman@repo.proxmox.com upload --product devel --dist $(UPLOAD_DIST)
