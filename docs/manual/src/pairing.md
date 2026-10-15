# Pairing

Linking adds a machine to the mesh, once per machine, with both in front of you: one shows a short code, you type the code on the other, both screens show the same small picture and three words, you confirm on both, and the new machine is a member. Every other machine in the mesh learns about it by itself.

## The relay comes first

Linking goes through the relay, so the mesh needs one first: see [The relay](relay.md). To try jaunt on one computer, [Try it on one machine](try-it.md) runs a local test relay.

## The first machine

On the first machine, say the desktop benzene:

```sh
jaunt relay set https://relay.example.net
jaunt up
jaunt link
```

- `jaunt relay set URL` stores the relay for the whole mesh. Every machine that links later receives it with the member list, so you set it once. `jaunt relay` with no argument shows the current one. A second URL (`jaunt relay set URL1 URL2`) lists two relays. `--ca FILE` trusts a relay certificate signed by your own certificate authority.
- `jaunt up` installs and starts the background service.
- `jaunt link` opens the pairing screen and shows a code like `4127-tremor-kudos`. Leave it running.

## The new machine

On the new machine, say the laptop toluene, after [installing](install.md) jaunt:

```sh
jaunt up
jaunt join --relay https://relay.example.net
```

`jaunt join` asks for the code; type it. Spaces instead of dashes are fine. The `--relay` flag is needed only because this machine knows no relay yet; the `JAUNT_RELAY` environment variable or `install.sh --relay URL` does the same.

Type the code when asked rather than putting it on the command line: a command line can be read by every other user of the machine while the join runs. The command-line form still works, with a warning.

## The sigil and the three words

When the code is right, both screens show the same small symmetric picture (the sigil) and the same three words, and ask whether they match.

![The inviting side of jaunt link: benzene linked by a dotted line to toluene, a pink and cyan sigil on the left, the words tremor, orbit, velvet, and the question "Same sigil and words on both screens?" with y and n](../captures/link-sigil-80x24-truecolor.png)

Look at both screens. If the picture and all three words are the same, press `y` on both. The screens then tick through the last steps, and the inviting side tells the rest of the mesh about the new machine.

**If they differ, press `n`.** Nothing is changed on either machine. Different pictures mean the two machines do not share a key. Run `jaunt link` again for a fresh code; if it happens again, do not link over that relay.

Compare the words, not only the picture: they are the harder part to forge.

To stop linking at any point, press Ctrl-c, or `n` at the question.

## How it works

The construction is magic-wormhole's, from standard parts.

1. **The code** is a nameplate (4 random digits) and two words from the EFF large wordlist, drawn from the operating system's random source. The digits are not secret; the words carry about 25.8 bits of secret.
2. **Meeting.** The inviting machine opens a temporary endpoint whose key is derived from the nameplate alone. The joining machine derives the same address and dials it through the relay. Anyone who guesses the nameplate can reach that endpoint, and all it gets them is one guess at the next step.
3. **SPAKE2**, a password-authenticated key exchange, runs with the whole code as the password. Someone in the middle gets exactly one guess: 1 in 60 million. A wrong guess burns the code. Codes expire after 5 minutes and work once.
4. **Sealed exchange.** From the shared key, each side derives an encryption key, then sends its real id and name with a signature proving it holds the key it claims.
5. **Human check.** The sigil and words are computed from the shared key, so both screens agree only when both sides hold the same key.
6. **Only after both confirmations** does the inviting machine send the roster and the relay list, and the new machine writes them.
7. A joining machine tries each code once, whatever went wrong.

The full description is §6 of `docs/design.md` in the repository, and the [Security model](security.md) lists what pairing defends against.

## When linking fails

| What you see | What to do |
| --- | --- |
| `jaunt join` reports `wrong code; that code is burnt now, ask for a new one (jaunt link)`, and `jaunt link` says `someone tried a wrong code; the code is burnt` | a typo, or someone else guessed. Run `jaunt link` again for a new code |
| `jaunt link` says `the code expired before anyone used it (codes last 5 minutes); run 'jaunt link' again` | as it says |
| `jaunt join` says `this code was already tried from this machine and is not tried twice; ask for a new one` | each code is tried once per joining machine; get a new one |
| `jaunt join` says `no relay is known here; give the one the other machine uses: jaunt join --relay URL` | this machine has no relay yet |
| `jaunt join` says `no inviting machine answered within ... s; is 'jaunt link' still showing this code, and do both machines use the same relay?` | check that `jaunt link` is still open (an expired code closes it) and that both sides name the same relay |

More in [Troubleshooting](troubleshooting.md).

## More machines

Each further machine joins the same way, from **any** machine already in the mesh, not only the first. Run `jaunt link` there and `jaunt join` on the new one. The new machine appears on every other member without you touching them.

Trust is flat: every member can link new machines and revoke others.

## Names

A machine's name defaults to its hostname in lower case. `install.sh --name` sets it at install time, and

```sh
jaunt name xylene
```

renames this machine later; the others see the new name. Names are lowercase letters, digits and `-`, at most 32 characters.

## Removing machines

| Command | Run it on | Does |
| --- | --- | --- |
| `jaunt unlink toluene` | any other member | revokes toluene: it is refused everywhere from now on, its connections close and the commands it was running end. Its tmux sessions are kept |
| `jaunt unlink toluene --kill-sessions` | any other member | the same, and ends the tmux sessions toluene created, here and on every machine that confirms |
| `jaunt unlink toluene --cascade` | any other member | also revokes every machine toluene linked |
| `jaunt leave` | the machine itself | takes this machine out of the mesh, tells the others, and deletes its key and roster |
| `jaunt reset` | the machine itself | forgets this machine's identity and roster and starts with a new key; link it again afterwards |

Revocation is permanent: a revoked key can never come back. A laptop you got back is `jaunt reset` and linked again under a new key. `unlink` prints which machines confirmed and which have not heard yet; a machine that has not heard still accepts the revoked one until it syncs with one that has. The whole story, including what revocation does not undo, is in [Revoking a lost laptop](security.md#revoking-a-lost-laptop).
