#!/bin/bash
# launchd wrapper for `tango task run --loop`.
# launchd does not inherit the shell environment, so keys come from ~/.tango/env
# (plain KEY=VALUE lines, chmod 600), e.g. TKNET_API_KEY=...
set -u
export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
[ -f "$HOME/.tango/env" ] && set -a && . "$HOME/.tango/env" && set +a
exec /opt/homebrew/bin/tango task run --loop
