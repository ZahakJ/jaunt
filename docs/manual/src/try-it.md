# Try it on one machine

The pairing, the hop and the mesh view can all be tried without a relay on the internet and without a second computer. The trick is two parts:

- **A local test relay.** `iroh-relay` has a development mode that serves plain HTTP. Bound to `127.0.0.1`, it is reachable only from this computer, and it needs no certificate.
- **Two pretend machines.** jaunt keeps everything it owns (key, roster, config, state, socket) under one directory when the `JAUNT_HOME` variable points there. Two such directories are two machines as far as jaunt is concerned, each with its own key and its own daemon.

One more variable, `JAUNT_TMUX_SOCKET`, puts the hop's tmux sessions on a separate tmux server, so the test never touches your own tmux sessions.

What you get from this: a real `jaunt link` and `jaunt join` with the code, the sigil and the three words; a real hop with the escape keys and re-attach; `jaunt ls` and the mesh view with two machines in them. What you do not get: anything about real networks. Both pretend machines are on the same computer, so it says nothing about home routers, relayed paths or latency. Use [What was tested](testing.md) for numbers, not this.

Some details worth knowing before you start:

- Each command below runs with `JAUNT_HOME` set for one machine. A command without it would talk to your real jaunt, if you have one.
- `jaunt link` and `jaunt join` need two terminals, both with the same `JAUNT_TMUX_SOCKET`.
- A plain-HTTP relay is for trying out only. jaunt accepts `http://` relay URLs for exactly this case; a real mesh uses the HTTPS relay from [The relay](relay.md).
- The cleanup at the end removes everything the test created.

The rest of this chapter is `docs/try-it.md` from the repository.

{{#include ../../try-it.md:2:}}
