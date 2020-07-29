.PHONY: all
all: check

.PHONY: check
check:
	cargo test --all-features

.PHONY: dinstall
dinstall: deb

.PHONY: deb
deb:
	rm -rf build
	mkdir build
	debcargo package \
	    --config "$(PWD)/debian/debcargo.toml" \
	    --changelog-ready \
	    --no-overlay-write-back \
	    --directory "$(PWD)/build/pxar" \
	    "pxar" \
	    "$$(dpkg-parsechangelog -l "debian/changelog" -SVersion | sed -e 's/-.*//')"
	echo system >build/rust-toolchain
	(cd build/pxar && CARGO=/usr/bin/cargo RUSTC=/usr/bin/rustc dpkg-buildpackage -b -uc -us)
	lintian build/*.deb

.PHONY: clean
clean:
	rm -rf build *.deb *.buildinfo *.changes *.orig.tar.gz
	cargo clean

upload: deb
	cd build; \
	    dcmd --deb rust-pxar_*.changes \
	    | grep -v '.changes$$' \
	    | tar -cf- -T- \
	    | ssh -X repoman@repo.proxmox.com upload --product devel --dist buster
