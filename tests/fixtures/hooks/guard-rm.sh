#!/bin/sh
# PreToolUse: refuse any bash call containing "rm -rf".
if grep -q 'rm -rf'; then
  echo 'guard: rm -rf is not allowed' >&2
  exit 2
fi
exit 0
