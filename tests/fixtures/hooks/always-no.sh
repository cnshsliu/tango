#!/bin/sh
# Any event: always block (used for the Stop cap and the chain-order test).
cat > /dev/null
echo "always-no $1" >&2
exit 2
