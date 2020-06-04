.PHONY: all
all: check

.PHONY: check
check:
	cargo test

.PHONY: dinstall
dinstall: deb

.PHONY: deb
deb:
	rm -rf build
	debcargo package \
	    --config "$(PWD)/debian/debcargo.toml" \
	    --changelog-ready \
	    --no-overlay-write-back \
	    --directory "$(PWD)/build" \
	    "pxar" \
	    "$$(dpkg-parsechangelog -l "debian/changelog" -SVersion | sed -e 's/-.*//')"
	echo system >build/rust-toolchain
	(cd build && CARGO=/usr/bin/cargo RUSTC=/usr/bin/rustc dpkg-buildpackage -b -uc -us)
	lintian *.deb

.PHONY: clean
clean:
	rm -rf build *.deb *.buildinfo *.changes *.orig.tar.gz
	cargo clean
