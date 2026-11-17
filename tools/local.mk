# Include from the assembled Tempo workspace root.
TEMPO_DB ?= .local/tactics-mvp/personal-practice.sqlite
TEMPO_PORT ?= 41821
TEMPO_EXECUTABLE := $(or $(CARGO_TARGET_DIR),target)/debug/tempo

.PHONY: start
start:
	@test -f "$(TEMPO_DB)" || { printf '%s\n' 'Practice database not found. See tempo/TACTICS.md for setup, or set TEMPO_DB.' >&2; exit 1; }
	@test -f tempo/ui/dist/index.html || { printf '%s\n' 'The Tempo UI has not been built. See tempo/TACTICS.md for build steps.' >&2; exit 1; }
	@python3 tempo/tools/stop_local_server.py "$(TEMPO_PORT)" "$(TEMPO_EXECUTABLE)"
	cargo run --locked -p tempo -- serve "$(TEMPO_DB)" tempo/ui/dist "$(TEMPO_PORT)"
