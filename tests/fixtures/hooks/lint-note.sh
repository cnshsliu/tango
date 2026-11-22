#!/bin/sh
# PostToolUse: pretend a linter found problems; exit 2 sends this to the model.
cat > /dev/null
echo 'lint: 2 warnings in hello.txt' >&2
exit 2
