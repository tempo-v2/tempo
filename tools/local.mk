# Include from the assembled Tempo workspace root.
TEMPO_PORT ?= 41823
export TEMPO_PORT
# TEMPO_DB is absent unless explicitly selected. Make command-line variables
# and variables inherited from the environment are exported automatically.
.PHONY: start build-ui init-dev
start build-ui init-dev:
	python3 -B tempo/tools/local_development.py $@
