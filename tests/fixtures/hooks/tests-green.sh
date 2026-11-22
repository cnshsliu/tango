#!/bin/sh
# Stop: send the model back once ("run the tests"), then let it finish.
if grep -q '"stop_hook_active":false'; then
  echo 'run the tests before finishing' >&2
  exit 2
fi
exit 0
