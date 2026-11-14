# SPDX-License-Identifier: GPL-3.0-or-later
#
#   make                  build release binaries (as your user)
#   sudo make install     install into $(PREFIX) + /usr/share/wayland-sessions
#   sudo make uninstall   remove the installation
#   make DESTDIR=pkg install   stage for packaging (no root needed)

PREFIX           ?= /usr/local
DESTDIR          ?=
SESSIONS_DIR     ?= /usr/share/wayland-sessions
SYSTEMD_USER_DIR ?= $(PREFIX)/lib/systemd/user
CARGO            ?= cargo
CARGO_FLAGS      ?=

INSTALL_ARGS = --prefix "$(PREFIX)" --sessions-dir "$(SESSIONS_DIR)" \
               --systemd-user-dir "$(SYSTEMD_USER_DIR)" \
               $(if $(DESTDIR),--destdir "$(DESTDIR)")

.PHONY: all build test check install uninstall clean

all: build

build:
	$(CARGO) build --release --locked $(CARGO_FLAGS) -p mochi-compositor -p mochi-ipc

test:
	$(CARGO) test --locked $(CARGO_FLAGS)

check: build
	./target/release/mochi-compositor --check-config --config config/mochi.kdl >/dev/null
	sh -n resources/mochi-session
	bash -n install.sh

install:
	./install.sh --no-build $(INSTALL_ARGS)

uninstall:
	./install.sh --uninstall $(INSTALL_ARGS)

clean:
	$(CARGO) clean

