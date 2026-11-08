# Sultan POS developer commands. Coverage gate: 80% workspace-wide (docs/plan.md §6).
.PHONY: all fmt fmt-check lint test cov cov-html check test-mysql

all: check

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

cov:
	cargo llvm-cov --workspace --fail-under-lines 80 </dev/null

cov-html:
	cargo llvm-cov --workspace --html </dev/null

check: fmt-check lint test cov

# Runs the whole test suite (SQLite and MySQL cases) against a throwaway MySQL 8.4 container
# (crates/db/README.md). Override the port with `make test-mysql MYSQL_PORT=33307`.
MYSQL_CONTAINER := sultan-mysql-test
MYSQL_PORT := 33306
MYSQL_TEST_URL := mysql://root:test@127.0.0.1:$(MYSQL_PORT)/sultan_test

test-mysql:
	docker rm -f $(MYSQL_CONTAINER) >/dev/null 2>&1 || true
	docker run -d --rm --name $(MYSQL_CONTAINER) -e MYSQL_ROOT_PASSWORD=test \
		-e MYSQL_DATABASE=sultan_test -p 127.0.0.1:$(MYSQL_PORT):3306 mysql:8.4 >/dev/null
	until docker exec $(MYSQL_CONTAINER) mysqladmin ping -h127.0.0.1 -uroot -ptest --silent \
		>/dev/null 2>&1 && docker logs $(MYSQL_CONTAINER) 2>&1 | grep -q "ready for connections.*port: 3306"; do \
		sleep 2; done
	SULTAN_TEST_MYSQL_URL=$(MYSQL_TEST_URL) cargo test --workspace; \
		status=$$?; docker stop $(MYSQL_CONTAINER) >/dev/null; exit $$status
