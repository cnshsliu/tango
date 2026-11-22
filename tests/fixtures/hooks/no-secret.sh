#!/bin/sh
# UserPromptSubmit: reject prompts that carry an API-key-looking token.
if grep -q 'sk-[A-Za-z0-9]'; then
  echo 'prompt contains an API key' >&2
  exit 2
fi
exit 0
