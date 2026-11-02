# Waggle

A lightweight EVE Online companion for Goonswarm pilots: a tray app on
Windows, and a bar widget on [Omarchy](https://omarchy.org).

Waggle does four things and nothing else:

- **Intel**: watches your intel channels' chat logs and warns you when a
  hostile is reported near one of your characters.
- **Pings**: Directorbot fleet pings over Jabber, as desktop notifications.
- **PAP count**: your participation numbers, fetched from Directorbot.
- **Mining meter**: what you've actually mined, read from your game logs.

It also tells you when a skill finishes or a skill queue runs dry.

It's meant to replace RIFT for people who only want these features, without
the memory use and slowness. On Windows it sits in the system tray. On Omarchy
it runs as a background service with a bar widget. Either way, its first run
can import your Jabber login, intel channels and jump bridges straight from
RIFT.

The name comes from the waggle dance: the way a scout bee tells the hive where
something is.

## Thanks to RIFT

Waggle owes a great deal to [RIFT Intel Fusion Tool](https://riftforeve.online)
by Nohus. RIFT showed what a desktop intel tool for EVE should do, and much of
Waggle's feature set comes straight from using it every day: intel alerts by
jump distance, Directorbot pings, PAP counts, jump bridges, and its ESI login.
Waggle is a separate, much smaller program written from scratch. It doesn't
copy RIFT's code, and it identifies itself as Waggle, never as RIFT. If you
want maps, killmails, and everything else RIFT offers, use RIFT.

## Status

**Not started.** Waggle is being split out of EVEmarchy, a private
Omarchy-only app where all of this already works on Linux. [PLAN.md](PLAN.md) is the full plan, including what moves,
what gets rewritten, and the decisions still open.
