# Measurements

This page is the raw output behind the keystroke numbers in [What was tested](testing.md): one run of the latency script, the reference round trips measured with `ping` in the same run, the method, and the caveats. Read the caveats before quoting any number. The script it names, `test/e2e/09-latency.sh`, and the harness, `test/harness`, are in the [repository](https://github.com/ZahakJ/jaunt).

How to read the keystroke table: "harness delay" is the delay added on every simulated router hop. A direct path between two machines crosses four such hops, a relayed path six, so at 20 ms a direct keystroke round trip cannot be faster than 80 ms and a relayed one cannot be faster than 120 ms. The 0 ms rows show what the software itself costs.

Three more things help with the tables:

- **Median and p95.** Each row is 200 keystrokes. The median is the typical one; the 95th percentile (p95) is the slow end, the time that 5 in 100 keystrokes took longer than. A small gap between the two means steady timing.
- **The two SSH rows.** OpenSSH by default hides the rhythm of your typing by sending extra packets every 20 ms and holding real keystrokes to that clock (`ObscureKeystrokeTiming`). In the three lab runs that added between about 3 and 18 ms per keystroke. jaunt does not do this, so the fair software comparison is with the option turned off.
- **Why there is no Cloudflare row.** The SSH rows stand in for SSH through a tunnel: the relay host plays the part of Cloudflare's edge. The real tunnel was not touched and not measured.

{{#include ../../measurements.md:2:}}
