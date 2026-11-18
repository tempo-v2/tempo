# Include from the assembled Tempo workspace root.
# Command-line variables and the environment can select development resources.
.PHONY: start start-dev build-ui init-dev
start:
	python3 -B tempo/tools/managed_start.py

start-dev build-ui init-dev:
	python3 -B tempo/tools/local_development.py $@
