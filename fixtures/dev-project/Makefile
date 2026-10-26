.PHONY: build test test-failing lint fmt-check changed diff-summary

build: ## Check that the project builds
	mkdir -p out
	echo "built" > out/artifact.txt

test: ## Run the project's test suite
	@sh tests/run.sh

test-failing: ## Run the known-failing regression test
	@sh tests/failing.sh

lint: ## Run lint checks
	@sh scripts/lint.sh

fmt-check: ## Check formatting
	@sh scripts/fmt-check.sh

changed: ## List changed files in the working tree
	@git status --short

diff-summary: ## Summarize the working tree diff
	@git diff --stat
