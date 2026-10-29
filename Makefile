#= Makefile — one front door to gliff's bin/ and scripts/ entry points.
#= Version: 0.2.0
#=
#= Every target calls the script that already does the job, with the
#= arguments that script expects; nothing here re-implements a build step.
#= Run `make` (or `make help`) for the list.
#=
#= Portable to GNU make 3.81 (the one macOS ships) and later.
#=
#= Logging: each target's output goes to the terminal and is appended to
#= target/logs/<target>.log. Add SYSLOG=1 to also send a start/finish line to
#= syslog (logger(1); on macOS: `log stream --predicate 'process == "logger"'`).
#= Add V=1 to echo the commands make runs.
#=
#= Knobs (pass on the command line, e.g. `make app APP=/tmp/Gliff.app`):
#=   ARGS                     extra arguments for the underlying script
#=   APP                      output of `make app` (default target/macos/Gliff.app)
#=   SIGN_MODE                development | release, for `make app`
#=   OUT                      output directory for `make package*`
#=   SSH_PORT                 port for `make discovery`
#=   GLIFF_CODESIGN_IDENTITY  signing fingerprint (else auto-detected; see docs/macos.md)
#=   GLIFF_NOTARY_PROFILE     notarytool keychain profile (release packaging only)
#=   CHECK=1                  `make macos`: run scripts/check.sh before installing
#=   SIGNING=0                `make macos`: skip creating the gliff-local identity

MAKEFILE_VERSION := 0.2.0

#= bash for pipefail, so `cmd | tee log` fails when cmd fails. Present on
#= macOS (/bin/bash 3.2) and every Linux this repo targets. make 3.81
#= ignores .SHELLFLAGS, so `run` also sets pipefail itself.
SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

ROOT := $(shell cd "$(dir $(lastword $(MAKEFILE_LIST)))" && pwd)
OS := $(shell uname -s)
LOG_DIR := $(ROOT)/target/logs

ARGS ?=
APP ?= $(ROOT)/target/macos/Gliff.app
SIGN_MODE ?= development
OUT ?=
SSH_PORT ?= 22
CHECK ?= 0
SIGNING ?= 1
#= Where bin/install-macos puts the app (fixed by that script).
INSTALLED_APP := $(HOME)/Applications/Gliff.app
SYSLOG ?= 0
V ?= 0

ifeq ($(V),1)
Q :=
else
Q := @
endif

#= ---------------------------------------------------------------------------
#= Helpers
#= ---------------------------------------------------------------------------

#= $(call run,NAME,COMMAND): run COMMAND from the repo root, tee its output
#= to target/logs/NAME.log with a timestamped header and result, and
#= optionally log start/finish to syslog. The exit status is preserved.
define run
	$(Q)mkdir -p "$(LOG_DIR)"
	$(Q)set -o pipefail; log="$(LOG_DIR)/$(1).log"; \
	stamp() { date '+%Y-%m-%dT%H:%M:%S'; }; \
	say() { \
	    printf '%s make[%s] %s\n' "$$(stamp)" "$(1)" "$$*" | tee -a "$$log" >&2; \
	    if [ "$(SYSLOG)" = 1 ] && command -v logger >/dev/null 2>&1; then \
	        logger -t gliff-make "[$(1)] $$*" || true; \
	    fi; \
	}; \
	say "start (V=1 shows the commands)"; \
	cd "$(ROOT)"; \
	if ( $(2) ) 2>&1 | tee -a "$$log"; then \
	    say "ok (log: $$log)"; \
	else \
	    status=$$?; say "FAILED with status $$status (log: $$log)"; exit $$status; \
	fi
endef

#= $(call need_os,Darwin|Linux,TARGET): stop early with a clear message.
define need_os
	$(Q)if [ "$(OS)" != "$(1)" ]; then \
	    echo "make $(2): needs $(1), this is $(OS)." >&2; exit 1; \
	fi
endef

#= $(call need_tool,TOOL,HINT)
define need_tool
	$(Q)command -v $(1) >/dev/null 2>&1 || { echo "missing tool: $(1) — $(2)" >&2; exit 1; }
endef

.PHONY: help version doctor \
        build check fix test e2e clean \
        macos verify video-check signing signing-show app install package package-release icon \
        dev install-dev discovery discovery-off \
        logs-clean

#= ---------------------------------------------------------------------------
#= Info
#= ---------------------------------------------------------------------------

help: ## Show this list
	@printf 'gliff make targets (Makefile %s, %s)\n\n' "$(MAKEFILE_VERSION)" "$(OS)"
	@awk 'BEGIN { FS = ":.*## " } \
	    /^#== / { sub(/^#== /, ""); printf "\n%s\n", $$0 } \
	    /^[a-z][a-z-]*:.*## / { printf "  %-16s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@printf '\nExamples:\n  make check\n  make signing install          # macOS, local-only signing\n  make package OUT=~/Desktop     # macOS, ad-hoc test ZIP\n  make build ARGS="-p gliff"     # pass cargo flags through\n'

version: ## Print the gliff and Makefile versions
	@printf 'gliff %s\nMakefile %s\n' \
	    "$$(sed -n 's/^version = "\(.*\)"/\1/p' "$(ROOT)/Cargo.toml" | head -1)" "$(MAKEFILE_VERSION)"

doctor: ## Check tools, cargo output location and signing setup
	@echo "== system";  uname -srm
	@echo "== tools"
	@for t in cargo rustc git; do \
	    printf '  %-10s %s\n' "$$t" "$$(command -v $$t || echo MISSING)"; done
	@if [ "$(OS)" = Darwin ]; then \
	    for t in xcrun codesign security plutil sips iconutil ditto; do \
	        printf '  %-10s %s\n' "$$t" "$$(command -v $$t || echo MISSING)"; done; \
	    printf '  %-10s %s\n' swiftc "$$(xcrun --find swiftc 2>/dev/null || echo 'MISSING (xcode-select --install)')"; \
	else \
	    for t in makepkg systemctl notify-send flock; do \
	        printf '  %-10s %s\n' "$$t" "$$(command -v $$t || echo -)"; done; \
	fi
	@echo "== cargo"
	@cd "$(ROOT)" && printf '  target dir: %s\n' \
	    "$$(cargo metadata --format-version 1 --no-deps 2>/dev/null | grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
	@printf '  CARGO_TARGET_DIR=%s CARGO_BUILD_TARGET=%s\n' "$${CARGO_TARGET_DIR:-}" "$${CARGO_BUILD_TARGET:-}"
	@if [ "$(OS)" = Darwin ]; then \
	    echo "== signing"; \
	    printf '  GLIFF_CODESIGN_IDENTITY=%s\n' "$${GLIFF_CODESIGN_IDENTITY:-(unset)}"; \
	    printf '  gliff-local: %s\n' "$$("$(ROOT)/bin/setup-macos-signing" --show 2>/dev/null || echo 'none (make signing)')"; \
	fi

#== Build and test (macOS and Linux)

build: ## Release build: gliff + gliff-server on macOS, everything on Linux (bin/build)
	$(call need_tool,cargo,install Rust with rustup)
	$(call run,build,bin/build $(ARGS))

check: ## CI checks: rustfmt, clippy -D warnings, unit tests (scripts/check.sh)
	$(call need_tool,cargo,install Rust with rustup)
	$(call run,check,scripts/check.sh)

fix: ## Let rustfmt and clippy apply fixes, then run the checks (scripts/check.sh --fix)
	$(call run,fix,scripts/check.sh --fix)

test: ## Unit tests only (cargo test --workspace --locked)
	$(call run,test,cargo test --workspace --locked $(ARGS))

e2e: ## Hardware end-to-end tests; needs a Hyprland session (scripts/e2e.sh)
	$(call need_os,Linux,e2e)
	$(call run,e2e,scripts/e2e.sh $(ARGS))

clean: ## cargo clean (removes target/, logs included)
	$(Q)cd "$(ROOT)" && cargo clean $(ARGS)

logs-clean: ## Remove target/logs
	$(Q)rm -rf "$(LOG_DIR)"

#== macOS

macos: ## Pipeline: signing identity, [CHECK=1: checks], build + sign + install, verify
	$(call need_os,Darwin,macos)
	@#= Each step is a separate make so the order holds even under -j, and
	@#= ARGS is cleared so one step's extra arguments never reach another.

	$(Q)if [ "$(SIGNING)" = 1 ]; then \
	    $(MAKE) --no-print-directory signing ARGS=; \
	else \
	    echo "make macos: SIGNING=0, using an existing identity or ad-hoc signing"; \
	fi
	$(Q)if [ "$(CHECK)" = 1 ]; then $(MAKE) --no-print-directory check ARGS=; fi
	$(Q)$(MAKE) --no-print-directory install ARGS=
	$(Q)$(MAKE) --no-print-directory verify
	@printf '\nGliff is installed at %s\nOpen it, or run: ~/.local/bin/gliff user@host\n' "$(INSTALLED_APP)"

#= Inside the run helper's command, write case patterns as (pattern): a bare
#= closing parenthesis would end make's call argument early.
verify: ## Check the installed app's signature and the ~/.local/bin links
	$(call need_os,Darwin,verify)
	$(call run,verify,\
	    app="$(INSTALLED_APP)"; \
	    [ -d "$$app" ] || { echo "no app at $$app (run: make install)"; exit 1; }; \
	    codesign --verify --deep --strict "$$app"; \
	    info=$$(codesign -dvv "$$app" 2>&1); \
	    printf '%s\n' "$$info" | grep -E '^(Identifier|Authority|Signature|TeamIdentifier)='; \
	    if printf '%s\n' "$$info" | grep -q '^Signature=adhoc'; then \
	        echo "warning: ad-hoc signed; privacy grants reset on every rebuild (run: make signing install)"; \
	    fi; \
	    for name in gliff gliff-server; do \
	        link="$(HOME)/.local/bin/$$name"; \
	        target=$$(readlink "$$link" 2>/dev/null || true); \
	        case "$$target" in \
	            ("$$app"/*) echo "$$link -> $$target" ;; \
	            (*) echo "$$link does not point into $$app"; exit 1 ;; \
	        esac; \
	    done; \
	    echo "signature and links OK")

video-check: ## GPU video check + GPU vs CPU decode times (needs an installed app; ARGS="--cases 1080p-444")
	$(call need_os,Darwin,video-check)
	$(call run,video-check,python3 scripts/macos-video-check.py --json "$(LOG_DIR)/video-check.json" $(ARGS))

signing: ## Create the local "gliff-local" signing identity once (bin/setup-macos-signing)
	$(call need_os,Darwin,signing)
	$(call run,signing,bin/setup-macos-signing --log "$(LOG_DIR)/signing.log" $(ARGS))

signing-show: ## Print the gliff-local fingerprint
	$(call need_os,Darwin,signing-show)
	$(Q)"$(ROOT)/bin/setup-macos-signing" --show

app: ## Build a signed Gliff.app at APP without installing (bin/build-macos-app)
	$(call need_os,Darwin,app)
	$(Q)case "$(SIGN_MODE)" in development|release) ;; \
	    *) echo "SIGN_MODE must be development or release" >&2; exit 1 ;; esac
	$(Q)case "$(APP)" in *.app) ;; *) echo "APP must end in .app" >&2; exit 1 ;; esac
	@#= The script refuses an existing bundle; only replace one under target/.
	$(Q)case "$(APP)" in "$(ROOT)"/target/*) rm -rf "$(APP)" ;; \
	    *) [ ! -e "$(APP)" ] || { echo "$(APP) exists; remove it or pick another APP" >&2; exit 1; } ;; esac
	$(Q)mkdir -p "$$(dirname "$(APP)")"
	$(call run,app,bin/build-macos-app "$(APP)" $(SIGN_MODE))

install: ## Install: ~/Applications/Gliff.app on macOS, makepkg on Arch (bin/install)
	$(call run,install,bin/install $(ARGS))

package: ## Ad-hoc signed test ZIP + SHA-256 (bin/package-macos --development)
	$(call need_os,Darwin,package)
	$(call run,package,bin/package-macos --development $(OUT))

package-release: ## Notarized ZIP; needs Developer ID + GLIFF_NOTARY_PROFILE (bin/package-macos)
	$(call need_os,Darwin,package-release)
	$(Q)[ -n "$${GLIFF_NOTARY_PROFILE:-}" ] || { echo "Set GLIFF_NOTARY_PROFILE (docs/macos.md); for local use run: make package" >&2; exit 1; }
	$(Q)[ -n "$${GLIFF_CODESIGN_IDENTITY:-}" ] || { echo "Set GLIFF_CODESIGN_IDENTITY to a Developer ID Application fingerprint" >&2; exit 1; }
	$(call run,package-release,bin/package-macos $(OUT))

icon: ## Regenerate crates/gliff-macos/resources/Gliff.icns (bin/export-macos-icon)
	$(call need_os,Darwin,icon)
	$(call run,icon,bin/export-macos-icon $(ARGS))

#== Linux

dev: ## Build and launch the checkout's client, logging to target/dev-*.log (bin/dev)
	$(call need_os,Linux,dev)
	$(Q)"$(ROOT)/bin/dev" $(ARGS)

install-dev: ## Add "Gliff (Development)" to the app launcher (bin/install-dev)
	$(call need_os,Linux,install-dev)
	$(call run,install-dev,bin/install-dev)

discovery: ## Advertise this desktop over mDNS on SSH_PORT (bin/enable-discovery)
	$(call need_os,Linux,discovery)
	$(Q)case "$(SSH_PORT)" in ''|*[!0-9]*) echo "SSH_PORT must be a number" >&2; exit 1 ;; esac
	$(call run,discovery,bin/enable-discovery --ssh-port $(SSH_PORT))

discovery-off: ## Stop advertising (bin/enable-discovery --disable)
	$(call need_os,Linux,discovery-off)
	$(call run,discovery,bin/enable-discovery --disable)
