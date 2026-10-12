# Decisions

Every tool is a pile of choices, and some of jaunt's were made for you during the build. This chapter lists them with the reason for each, so you can disagree knowingly, and then lists the choices that were left to you.

A few of them shape everything else and are worth reading first:

- **iroh for the transport.** jaunt writes no networking or cryptography of its own. The cost is that iroh is young and has no published audit.
- **A built-in shell server, one layer by default.** Hops work on machines with no sshd and start instantly. The research behind jaunt recommended carrying SSH by default; that is available as `jaunt pipe` instead, for the machines where two layers matter.
- **Flat trust.** Every member can link and revoke. No master key to lose, and one stolen, unlocked machine is the whole mesh until it is revoked.
- **No pairing without a relay.** The design mentioned linking two machines on one network without a relay; that was not built.
- **The relay is left open.** No id allowlist and no token, because an allowlist breaks pairing and jaunt does not send relay tokens.

Under "Yours to make", one item has since been decided: the repository has been public since 11 October 2026, and the install line no longer needs the GitHub CLI.

The relay hosting notes the first item there points to are the [Hosting notes](relay.md#hosting-notes) in the relay chapter of this manual; the link inside the included text points at the repository's file and does not work from this page.

Two other defaults lean towards changing nothing you did not ask for. Port mapping (asking the home router through UPnP, NAT-PMP or PCP to forward a port) is off, because it changes the router's settings; `jaunt check` mentions it when no path is direct. And a revoked machine's tmux sessions are kept unless you pass `--kill-sessions`, because they are yours and may hold your work.

The rest of this chapter is `docs/decisions.md` from the repository.

{{#include ../../decisions.md:2:}}
