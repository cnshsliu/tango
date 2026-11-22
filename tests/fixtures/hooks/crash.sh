#!/bin/sh
# A broken hook: exit 1 must warn, never block.
echo 'crash: something broke' >&2
exit 1
