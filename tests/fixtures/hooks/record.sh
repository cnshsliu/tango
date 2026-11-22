#!/bin/sh
# Append the event JSON (one line) to the log file given as $1, then allow.
cat >> "$1"
echo >> "$1"
exit 0
