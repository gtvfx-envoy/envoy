#!/bin/sh
# Removes the envoy launcher installed by install-envoy.sh.
#
# Takes the same --remove-cache and --yes options as `install-envoy.sh --uninstall`.

exec sh "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/install-envoy.sh" --uninstall "$@"
