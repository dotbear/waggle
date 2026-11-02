# Synthetic Directorbot fixtures

Every file here is hand-written to exercise a message shape. None of it is a
real alliance broadcast: the fleet names, characters, structures and comms
channels are invented, and no real PAP figures appear.

`account_info_arrows.txt` reproduces the `-> ` prefixes Directorbot puts on
each statistics line, using invented counts and a synthetic character. It
covers all three periods and the last-strat line, including multi-digit PCT
counts.

Never commit real alliance messages or EVE logs as fixtures, and never copy
another tool's test data. These exist so the parser's behaviour on format
variation is pinned without either.
