# Security policy

## Supported versions

oxenDB has not had a release yet. Until 1.0, only the latest commit on
`main` receives fixes.

## Reporting a vulnerability

Please do not open a public issue for security problems. Use GitHub's
private vulnerability reporting ("Report a vulnerability" under the
repository's Security tab) instead.

Include what you found, how to reproduce it, and what an attacker could do
with it. You should get an acknowledgement within a week.

## What counts

For an embedded database, the most likely serious issues are:

- A crafted database or WAL file that causes memory unsafety, a hang, or
  unbounded memory use when opened
- A crafted SQL statement that does the same
- Committed data being silently lost or altered

Opening a database file is currently assumed to be as dangerous as running
code from whoever wrote the file. Hardening against malicious files is a goal
(see the fuzzing items in `docs/release-criteria.md`), but it is not yet
guaranteed.
