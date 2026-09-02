SHELL := /bin/bash
.DEFAULT_GOAL := help

PACKAGES := treasury-registry stablecoin-registry dvp-settlement bridge-gateway bridge-tests integration-control integration tests
EXPECTED_TESTS := 194
DPM_HOME ?= $(HOME)/.dpm
REPO := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
.PHONY: help build test lint validate integration verify clean distclean whitespace bash-syntax dep-audit require-dpm require-java require-canton require-bridge-tools bridge-build bridge-test bridge-verify

help:
	@echo "build         compile every Daml package in multi-package.yaml"
	@echo "test          run the $(EXPECTED_TESTS) Daml Script unit tests"
	@echo "lint          run damlc lint over every project Daml module"
	@echo "validate      validate every project DAR and pin-check the vendored Token Standard DARs"
	@echo "integration   run the two-synchronizer Canton integration suite"
	@echo "bridge-build  compile Solana, Zama, bridge Daml and the Rust coordinator"
	@echo "bridge-test   run Solana, Zama, bridge Daml, Rust, format, clippy, and typecheck"
	@echo "bridge-verify original Canton verify plus every bridge gate"
	@echo "verify        original Canton DvP: build, test, lint, validate, integration, whitespace"
	@echo "dep-audit     production npm audit and RustSec checks"
	@echo "clean         remove generated build output and local runtime state"
	@echo "distclean     clean, plus zama/node_modules and the downloaded .cache source"

require-dpm:
	@command -v dpm >/dev/null 2>&1 || { echo "dpm not found on PATH; install the Daml SDK toolchain and add $(DPM_HOME)/bin to PATH" >&2; exit 1; }

require-java:
	@command -v java >/dev/null 2>&1 || { echo "java not found on PATH; JDK 21 is required to run Canton" >&2; exit 1; }

require-canton:
	@ls $(DPM_HOME)/cache/components/canton-open-source/*/lib/canton-open-source-*.jar >/dev/null 2>&1 || \
	  { echo "canton runtime not found under $(DPM_HOME)/cache/components/canton-open-source; run: dpm build --all" >&2; exit 1; }

require-bridge-tools:
	@command -v cargo >/dev/null 2>&1 || { echo "cargo not found" >&2; exit 1; }
	@command -v solana >/dev/null 2>&1 || { echo "solana CLI not found" >&2; exit 1; }
	@command -v anchor >/dev/null 2>&1 || { echo "anchor not found" >&2; exit 1; }
	@command -v node >/dev/null 2>&1 || { echo "node not found" >&2; exit 1; }
	@node -e 'const n=process.versions.node.split(".")[0]; if (n!=="22") { console.error("Node 22 is required"); process.exit(1); }'
	@test -d zama/node_modules || { echo "zama dependencies are not installed; run: (cd zama && npm ci)" >&2; exit 1; }

build: require-dpm
	@dpm build --all

test: build
	@out=$$(cd daml/tests && dpm test 2>&1) || { echo "$$out" >&2; exit 1; }; \
	 passed=$$(printf '%s\n' "$$out" | grep -c ': ok, ' || true); \
	 if [ "$$passed" -ne $(EXPECTED_TESTS) ]; then \
	   echo "expected $(EXPECTED_TESTS) passing Daml Script tests but counted $$passed" >&2; exit 1; \
	 fi; \
	 echo "TESTS_PASSED $$passed"

lint: build
	@for package in $(PACKAGES); do \
	  for module in $$(find daml/$$package/daml -name '*.daml'); do \
	    hints=$$(cd daml/$$package && dpm damlc lint $${module#daml/$$package/} 2>&1 | grep -v '^No hints$$' || true); \
	    if [ -n "$$hints" ]; then echo "$$module"; echo "$$hints"; exit 1; fi; \
	  done; \
	done
	@echo "LINT_CLEAN"

validate: build
	@for dar in daml/*/.daml/dist/*.dar lib/*.dar; do \
	  dpm damlc validate-dar $$dar >/dev/null || { echo "invalid dar: $$dar" >&2; exit 1; }; \
	done
	@cd lib && { command -v shasum >/dev/null 2>&1 && shasum -a 256 -c CHECKSUMS.sha256 || sha256sum -c CHECKSUMS.sha256; }
	@echo "DARS_VALID"

integration: build require-java require-canton
	@./canton/run-integration.sh

whitespace:
	@git diff --check HEAD
	@dirty=$$(git ls-files --cached --others --exclude-standard -- \
	  '*.daml' '*.canton' '*.md' '*.conf' '*.sh' '*.yaml' '*.yml' '*.json' \
	  '*.rs' '*.ts' '*.sol' '*.toml' 'Cargo.lock' 'package-lock.json' Makefile \
	  | xargs grep -slE '[[:space:]]+$$' || true); \
	 if [ -n "$$dirty" ]; then echo "trailing whitespace in:"; echo "$$dirty"; exit 1; fi
	@echo "WHITESPACE_CLEAN"

bash-syntax:
	@git ls-files --cached --others --exclude-standard -- '*.sh' | while IFS= read -r script; do \
	  bash -n "$$script" || exit 1; \
	done
	@if command -v shellcheck >/dev/null 2>&1; then \
	  git ls-files --cached --others --exclude-standard -- '*.sh' | xargs shellcheck; \
	fi
	@echo "BASH_SYNTAX_OK"

verify: build test lint validate integration whitespace
	@echo "VERIFY_COMPLETE"

bridge-build: require-dpm require-bridge-tools build
	@./scripts/build-token-2022-zk-ops.sh
	@cd solana && cargo test --manifest-path programs/confidential-escrow/Cargo.toml --lib --no-run
	@cd solana && anchor build
	@cd zama && npx hardhat compile
	@cd bridge/devnet-zk && cargo test --lib --no-run
	@cd bridge && cargo build
	@echo "BRIDGE_BUILD_COMPLETE"

bridge-test: bridge-build
	@cd solana && cargo fmt --check
	@cd solana && cargo clippy --manifest-path programs/confidential-escrow/Cargo.toml --all-targets -- -D clippy::all
	@cd solana && SBF_OUT_DIR="$(CURDIR)/solana/target/deploy" cargo test --manifest-path programs/confidential-escrow/Cargo.toml
	@cd zama && npx hardhat test
	@cd zama && npm run typecheck
	@cd daml/bridge-tests && dpm test
	@cd bridge/devnet-zk && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
	@cd bridge && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
	@echo "BRIDGE_TEST_COMPLETE"

dep-audit:
	@./scripts/bridge-dep-audit.sh

bridge-verify: verify bridge-test
	@./scripts/bridge-e2e.sh
	@./scripts/bridge-license-check.sh
	@./scripts/bridge-secret-scan.sh
	@./scripts/bridge-dep-audit.sh
	@$(MAKE) bash-syntax
	@git diff --check HEAD
	@echo "BRIDGE_VERIFY_COMPLETE"

define rm_generated
	target="$(1)"; \
	case "$$target" in \
	  "$(REPO)"/*) ;; \
	  *) echo "refusing to remove path outside the repository: $$target" >&2; exit 1 ;; \
	esac; \
	case "$$target" in \
	  "$(REPO)/daml/"*"/.daml"|"$(REPO)/bridge/target"|"$(REPO)/solana/target"|"$(REPO)/solana/.anchor"|"$(REPO)/solana/test-ledger"|"$(REPO)/zama/artifacts"|"$(REPO)/zama/cache"|"$(REPO)/zama/typechain-types"|"$(REPO)/zama/fhevmTemp"|"$(REPO)/zama/coverage"|"$(REPO)/zama/.openzeppelin"|"$(REPO)/canton/.run"|"$(REPO)/canton/.run-bridge"|"$(REPO)/canton/.run-walkthrough"|"$(REPO)/bridge/.run"|"$(REPO)/log"|"$(REPO)/zama/node_modules"|"$(REPO)/.cache") ;; \
	  *) echo "refusing to remove unlisted path: $$target" >&2; exit 1 ;; \
	esac; \
	if [ -e "$$target" ]; then rm -rf "$$target"; fi
endef

clean:
	@for package in $(PACKAGES); do $(call rm_generated,$(REPO)/daml/$$package/.daml); done
	@$(call rm_generated,$(REPO)/bridge/target)
	@$(call rm_generated,$(REPO)/solana/target)
	@$(call rm_generated,$(REPO)/solana/.anchor)
	@$(call rm_generated,$(REPO)/solana/test-ledger)
	@$(call rm_generated,$(REPO)/zama/artifacts)
	@$(call rm_generated,$(REPO)/zama/cache)
	@$(call rm_generated,$(REPO)/zama/typechain-types)
	@$(call rm_generated,$(REPO)/zama/fhevmTemp)
	@$(call rm_generated,$(REPO)/zama/coverage)
	@$(call rm_generated,$(REPO)/zama/.openzeppelin)
	@$(call rm_generated,$(REPO)/canton/.run)
	@$(call rm_generated,$(REPO)/canton/.run-bridge)
	@$(call rm_generated,$(REPO)/canton/.run-walkthrough)
	@$(call rm_generated,$(REPO)/bridge/.run)
	@$(call rm_generated,$(REPO)/log)
	@echo "CLEAN_COMPLETE"

distclean: clean
	@$(call rm_generated,$(REPO)/zama/node_modules)
	@$(call rm_generated,$(REPO)/.cache)
	@echo "DISTCLEAN_COMPLETE"
