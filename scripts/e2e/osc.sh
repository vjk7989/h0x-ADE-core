#!/usr/bin/env bash
# A program that reports its status over OSC 7501 the way an agent does:
# working, then blocked (a question), child records with control characters in
# their titles, then done. Each state is held for a few seconds so the test can see it.
b64() { printf '%s' "$1" | base64 | tr -d '\n'; }
rep() { printf '\033]7501;%s\033\\' "$1"; }

rep "state=working:app=e2e:msg=$(b64 building)"
sleep 6
rep "state=blocked:kind=question:app=e2e:msg=$(b64 'Which target?')"
sleep 6
# a, RIGHT-TO-LEFT OVERRIDE, b, ZERO WIDTH SPACE, c, LEFT-TO-RIGHT ISOLATE, d
bidi=$(printf 'a\xe2\x80\xaeb\xe2\x80\x8bc\xe2\x81\xa6d')
# "fam " and a family emoji joined by ZERO WIDTH JOINERs
zwj=$(printf 'fam \xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x91\xa7')
rep "state=working:id=w/bidi:title=$(b64 "$bidi")"
rep "state=working:id=w/zwj:title=$(b64 "$zwj")"
# a BEL in a title: the whole report is dropped
rep "state=working:id=w/bel:title=$(b64 "$(printf 'x\ay')")"
sleep 6
rep "state=done:app=e2e:msg=$(b64 'all done')"
sleep 1
