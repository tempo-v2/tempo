# Include from the assembled Tempo workspace root.
TEMPO_DEFAULT_DB := .local/usable-release/study.sqlite
TEMPO_DB ?= $(TEMPO_DEFAULT_DB)
TEMPO_PORT ?= 41822
TEMPO_EXECUTABLE := $(or $(CARGO_TARGET_DIR),target)/debug/tempo
TEMPO_UI := tempo-ui
TEMPO_UI_SOURCE := $(wildcard $(TEMPO_UI)/*.jsx $(TEMPO_UI)/*.js $(TEMPO_UI)/*.css $(TEMPO_UI)/build.mjs)
TEMPO_UI_DEPS := $(wildcard $(TEMPO_UI)/vendor/*.tgz) $(TEMPO_UI)/package.json $(TEMPO_UI)/package-lock.json

.PHONY: start build-ui
build-ui: $(TEMPO_UI)/dist/index.html

$(TEMPO_UI)/node_modules/.tempo-installed: $(TEMPO_UI_DEPS)
	cd "$(TEMPO_UI)" && npm ci --no-audit
	touch "$@"

$(TEMPO_UI)/dist/index.html: $(TEMPO_UI_SOURCE) $(TEMPO_UI)/node_modules/.tempo-installed
	cd "$(TEMPO_UI)" && npm run build

start: build-ui
	cargo build --locked -p tempo
	@if [ ! -f "$(TEMPO_DB)" ]; then \
		if [ "$(TEMPO_DB)" != "$(TEMPO_DEFAULT_DB)" ]; then \
			printf '%s\n' 'Selected database not found. Run tempo init DATABASE to create it.' >&2; exit 1; \
		fi; \
		umask 077; mkdir -p "$(dir $(TEMPO_DEFAULT_DB))" && "$(TEMPO_EXECUTABLE)" init "$(TEMPO_DEFAULT_DB)"; \
	fi
	@python3 tempo/tools/stop_local_server.py "$(TEMPO_PORT)" "$(TEMPO_EXECUTABLE)"
	"$(TEMPO_EXECUTABLE)" serve "$(TEMPO_DB)" "$(TEMPO_UI)/dist" "$(TEMPO_PORT)"
