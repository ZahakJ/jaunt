# Security model

jaunt gives a shell as you on every machine in the mesh. Whoever can use it on one machine can use it on all of them, so this chapter is the one to read before you link anything you care about.

The short version, in four sentences. The network and the relay are not trusted: traffic is encrypted end to end between the machines' own keys, and a machine whose key is not on the member list is refused during the handshake, before any jaunt service runs. The machines on the list are trusted completely and equally, so a stolen laptop that is unlocked and not yet revoked is a full member. Nothing here has had an outside security review: not jaunt, not iroh and its QUIC library, not the SPAKE2 crate used for pairing, and releases are not signed. Keep your existing way into your machines (for example SSH through a Cloudflare tunnel) until jaunt has a track record on your own machines.

Three practical consequences:

- **Use disk encryption and a screen lock** on every laptop in the mesh. The key file has no passphrase, because the daemon must start unattended, so the disk and the screen lock are what protect it.
- **Know how to revoke.** `jaunt unlink <name> --kill-sessions` from any other machine. The section [Revoking a lost laptop](#revoking-a-lost-laptop) below covers the window during which a machine that has not heard yet still accepts the revoked one, and what revocation cannot undo.
- **Add a second layer where it matters.** jaunt is a single layer by default. Where a machine runs sshd on its loopback, `ProxyCommand jaunt pipe %h` puts real OpenSSH on top, with its own key check behind jaunt's.

The section on Tailscale below is worth reading even if you have already decided: for many people Tailscale is the better tool, and the reasons are stated plainly.

The research survey the text below refers to is in the repository at [`docs/research/survey.md`](https://github.com/ZahakJ/jaunt/blob/main/docs/research/survey.md). The rest of this chapter is `docs/security.md` from the repository.

{{#include ../../security.md:2:}}
