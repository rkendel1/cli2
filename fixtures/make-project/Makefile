.PHONY: build test clean sleepy fail-hard install-system

# Build the artifact into out/
build:
	mkdir -p out
	echo "built" > out/artifact.txt

test: ## Run the checks
	@test -f Makefile && echo "checks passed"

clean: ## Remove build output
	rm -rf out

sleepy: ## Wait a long time (used to exercise timeouts and cancellation)
	sleep 30

fail-hard: ## Always fails
	@echo "about to fail" && exit 3

install-system: ## Install into system directories
	sudo cp out/artifact.txt /usr/local/share/
