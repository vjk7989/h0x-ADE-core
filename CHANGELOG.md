# Changelog

## [Unreleased]

## [0.1.3] - 2026-11-05

### Changed

- Faster terminal output: fewer copies and system calls between terminals and the apps watching them.
- The notices readme names h0x-core.

## [0.1.2] - 2026-11-05

### Fixed

- Fixed (security): a remote machine you ran a command on could gain the right to type into any of your local terminals; now only the terminal that called it.
- Remote machines can no longer force input into a terminal waiting on you, and their input is always labelled with the machine.
- Remote machines can no longer show their terminals as yours.
- Pasted text can no longer end bracketed paste early.
- Release files must be signed for that exact file, and a release list for another version is refused.

## [0.1.1] - 2026-11-05

### New

- Updates are signed: h0x-core checks a minisign signature on every release it downloads.
- Downloads go only over HTTPS and only from GitHub's release hosts.

### Fixed

- ssh and scp targets can no longer be read as options.
- Socket folders are private to your user and checked for their owner, sockets are readable by you alone (0600), and on Windows h0x-core refuses a pipe served by another account.
- The release-URL and ssh overrides work only in debug and test builds.
