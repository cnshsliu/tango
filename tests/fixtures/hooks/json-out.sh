#!/bin/sh
# Any event: print $1 (a JSON object) on stdout and exit 0 — the stdout-JSON protocol.
cat > /dev/null
printf '%s\n' "$1"
exit 0
