SHELL := /bin/bash
.DEFAULT_GOAL := help

PACKAGES := treasury-registry stablecoin-registry dvp-settlement integration-control integration tests
EXPECTED_TESTS := 194
DPM_HOME ?= $(HOME)/.dpm

.PHONY: help build test lint validate integration verify clean whitespace require-dpm require-java require-canton

help:
	@echo "build        compile every Daml package in multi-package.yaml"
	@echo "test         run the $(EXPECTED_TESTS) Daml Script unit tests"
	@echo "lint         run damlc lint over every project Daml module"
	@echo "validate     validate every project DAR and pin-check the vendored Token Standard DARs"
	@echo "integration  run the two-synchronizer Canton integration suite"
	@echo "verify       build, test, lint, validate, integration, whitespace"
	@echo "clean        remove generated build output and local Canton runtime state"

require-dpm:
	@command -v dpm >/dev/null 2>&1 || { echo "dpm not found on PATH; install the Daml SDK toolchain and add $(DPM_HOME)/bin to PATH" >&2; exit 1; }

require-java:
	@command -v java >/dev/null 2>&1 || { echo "java not found on PATH; JDK 21 is required to run Canton" >&2; exit 1; }

require-canton:
	@ls $(DPM_HOME)/cache/components/canton-open-source/*/lib/canton-open-source-*.jar >/dev/null 2>&1 || \
	  { echo "canton runtime not found under $(DPM_HOME)/cache/components/canton-open-source; run: dpm build --all" >&2; exit 1; }

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
	@dirty=$$(git ls-files --cached --others --exclude-standard -- '*.daml' '*.canton' '*.md' '*.conf' '*.sh' '*.yaml' '*.json' Makefile | xargs grep -slE '[[:space:]]+$$' || true); \
	 if [ -n "$$dirty" ]; then echo "trailing whitespace in:"; echo "$$dirty"; exit 1; fi
	@echo "WHITESPACE_CLEAN"

verify: build test lint validate integration whitespace
	@echo "VERIFY_COMPLETE"

clean:
	@rm -rf daml/*/.daml canton/.run log
	@echo "CLEAN_COMPLETE"
