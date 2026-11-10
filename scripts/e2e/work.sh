#!/usr/bin/env bash
# Reports `working` for a while, then `done` (for the keep_awake check).
printf '\033]7501;state=working:app=e2e:msg=%s\033\\' "$(printf '%s' busy | base64 | tr -d '\n')"
sleep "${1:-12}"
printf '\033]7501;state=done:app=e2e\033\\'
sleep 1
