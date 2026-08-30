PREFIX ?= $(HOME)/.local
BINDIR  = $(DESTDIR)$(PREFIX)/bin
APPDIR  = $(DESTDIR)$(PREFIX)/share/applications

.PHONY: all build install uninstall test clean run

all: build

build:
	cargo build --release

test:
	cargo test

# Installs to ~/.local by default, which needs no root. Use
# `sudo make install PREFIX=/usr/local` for a system-wide install.
install: build
	install -Dm755 target/release/fileman $(BINDIR)/fileman
	install -Dm644 data/dev.fileman.Files.desktop $(APPDIR)/dev.fileman.Files.desktop
	-update-desktop-database $(APPDIR) 2>/dev/null
	@echo "Installed to $(BINDIR)/fileman"
	@case ":$$PATH:" in *":$(PREFIX)/bin:"*) ;; \
	  *) echo "Note: $(PREFIX)/bin is not on your PATH.";; esac

uninstall:
	rm -f $(BINDIR)/fileman $(APPDIR)/dev.fileman.Files.desktop
	-update-desktop-database $(APPDIR) 2>/dev/null

run:
	cargo run --release

clean:
	cargo clean
