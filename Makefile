# bateri-browser: the hidden Chromium helper.
#
#   make check        formatting, lints and the fast tests (no engine is started)
#   make check-heavy  builds the app and runs the helper under the fake bateri, end to end
#   make app          the signed app bundle: target/$(PROFILE)/app-bundle/bateri-browser.app
#   make release      the notarized archive, SHA256SUMS and release notice in dist/ (nothing is
#                     pushed or published)
#
# `check-heavy` and `app` build with the same cargo profile (`PROFILE`, debug unless `release`
# is asked for) so that the engine's bindings are built once.

SHELL := /bin/bash

PROFILE ?= debug
CARGO_PROFILE = $(if $(filter release,$(PROFILE)),--release,)
OUT = target/$(PROFILE)
BUNDLE_DIR = $(OUT)/app-bundle
APP = $(BUNDLE_DIR)/bateri-browser.app
ENTITLEMENTS = assets/helper.entitlements
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
ARCH := $(shell uname -m | sed 's/aarch64/arm64/')

# cmake and ninja build the engine's C++ wrapper the first time.
export PATH := /opt/homebrew/bin:$(PATH)

# The signing identity is chosen from the keychain, like bateri's own package does: a valid
# "Developer ID Application", else "Apple Development", else ad-hoc (`-`). The certificate's name
# carries a person's e-mail, so it is never written in the repository. `SIGN_ID=-` forces ad-hoc.
SIGN_ID ?= $(eval SIGN_ID := $$(shell ids=$$$$(security find-identity -v -p codesigning 2>/dev/null); \
	for kind in "Developer ID Application" "Apple Development"; do \
		n=$$$$(printf '%s\n' "$$$$ids" | sed -n "s/.*\"\($$$$kind: [^\"]*\)\".*/\1/p" | head -n 1); \
		[ -n "$$$$n" ] && { echo "$$$$n"; exit 0; }; \
	done; echo -))$(SIGN_ID)
# The hardened runtime (notarization asks for it); a secure timestamp only with Developer ID.
SIGN_OPTS = $(if $(filter -,$(SIGN_ID)),--timestamp=none,--options runtime $(if $(findstring Developer ID Application:,$(SIGN_ID)),--timestamp,--timestamp=none))
SIGN = codesign --force --sign '$(SIGN_ID)' $(SIGN_OPTS)
NOTARY_PROFILE ?= bateri-notary

.PHONY: check fmt clippy test check-heavy build app sign release

check: fmt clippy test

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --locked -- -D warnings

# The end-to-end tests are `#[ignore]`d here: they need the bundle (`check-heavy`).
test:
	cargo test --workspace --all-targets --locked

build:
	cargo build $(CARGO_PROFILE) --locked -p browser-helper

# Chromium's processes are sandboxed (the renderer cannot read the user's files); the helper has
# no way to start without that, so the tests run the bundle, never a loose binary.
check-heavy: app
	BROWSER_APP=$(abspath $(APP)) cargo test $(CARGO_PROFILE) --locked -p browser-fake -- --ignored --test-threads=1

app: build
	cargo run $(CARGO_PROFILE) --locked -p browser-helper --bin bundle -- \
		--target-dir $(OUT) --out $(BUNDLE_DIR) --version $(VERSION) --license LICENSE
	@test '$(SIGN_ID)' = - || security find-identity -p codesigning | grep -qF '"$(SIGN_ID)"' || \
		{ echo "app: there is no code-signing identity named '$(SIGN_ID)' in the keychain"; exit 1; }
	$(MAKE) --no-print-directory sign
	codesign --verify --deep --strict '$(APP)'
	@echo "app: $(APP) (signed with '$(SIGN_ID)')"

# Inside out: the outer signature does not sign what is inside it.
sign:
	fw='$(APP)/Contents/Frameworks/Chromium Embedded Framework.framework'; \
	for lib in "$$fw"/Libraries/*.dylib; do $(SIGN) "$$lib"; done; \
	$(SIGN) "$$fw"; \
	for helper in '$(APP)'/Contents/Frameworks/*.app; do \
		$(SIGN) --entitlements $(ENTITLEMENTS) "$$helper"; \
	done; \
	$(SIGN) --entitlements $(ENTITLEMENTS) '$(APP)'

# Builds in release, signs, sends the app to Apple, staples, and writes what a release holds.
# Nothing is pushed or published: that is a decision of its own.
release:
	@printf '%s' '$(SIGN_ID)' | grep -q '^Developer ID Application:' || \
		{ echo "release: needs a Developer ID Application identity (found '$(SIGN_ID)')"; exit 1; }
	@git diff --quiet && git diff --cached --quiet || { echo "release: the tree is not clean"; exit 1; }
	$(MAKE) --no-print-directory PROFILE=release app
	rm -rf dist && mkdir -p dist
	ditto -c -k --sequesterRsrc --keepParent target/release/app-bundle/bateri-browser.app dist/notarize.zip
	out=$$(xcrun notarytool submit dist/notarize.zip --keychain-profile '$(NOTARY_PROFILE)' --wait 2>&1) || true; \
	echo "$$out" | tail -n 2; \
	echo "$$out" | grep -q 'status: Accepted' || { echo "release: notarization was not accepted"; exit 1; }
	xcrun stapler staple -q target/release/app-bundle/bateri-browser.app
	xcrun stapler validate -q target/release/app-bundle/bateri-browser.app
	spctl -a -vv -t exec target/release/app-bundle/bateri-browser.app 2>&1 | grep -q 'source=Notarized Developer ID' || \
		{ echo "release: Gatekeeper does not see the app as notarized"; exit 1; }
	rm dist/notarize.zip
	ditto -c -k --sequesterRsrc --keepParent target/release/app-bundle/bateri-browser.app dist/bateri-browser-$(VERSION)-$(ARCH).zip
	cd dist && shasum -a 256 bateri-browser-$(VERSION)-$(ARCH).zip > SHA256SUMS
	cargo run --release --locked -p browser-ipc --example notice -- \
		--version $(VERSION) --chromium "$$(scripts/chromium-version.sh)" --arch $(ARCH) \
		--name bateri-browser-$(VERSION)-$(ARCH).zip \
		--size "$$(stat -f%z dist/bateri-browser-$(VERSION)-$(ARCH).zip)" \
		--sha256 "$$(shasum -a 256 dist/bateri-browser-$(VERSION)-$(ARCH).zip | cut -d' ' -f1)" > dist/notice.json
	@echo "release: dist/ holds the archive, SHA256SUMS and notice.json (nothing was pushed)"
