#!/bin/sh
set -eu

# Development only: skip authentication. The engine still requires --in-memory.
if [ "${VAUBAN_NO_AUTH:-}" = "1" ]; then
  exec vauban serve --in-memory --data /var/lib/vauban --no-auth "$@"
fi

if [ -z "${VAUBAN_SA_PASSWORD:-}" ]; then
  echo "error: VAUBAN_SA_PASSWORD is required (or set VAUBAN_NO_AUTH=1 for development)" >&2
  exit 2
fi

exec vauban serve --in-memory --data /var/lib/vauban "$@"
