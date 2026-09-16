# Automatic download detection

Chiù can request idle-sleep protection when aggregate incoming network
activity stays meaningful for long enough. It observes received-byte counters
and elapsed time, not packets, files, applications, or processes.

## The detector cycle

The tray presents three phases:

1. **Waiting** — activity is below the threshold or has not stayed above it long
   enough. Qualification happens within this phase; it is not a separate tray
   state.
2. **Active** — meaningful receive activity has qualified, so the detector has
   automatic protection intent.
3. **Grace** — activity became quiet, but the qualified intent is retained for
   a limited time. The detector calls this `Hold`; the tray shows `Grace — …`
   and may describe the reason as `Recent network activity`.

Qualification and grace use actual observed durations rather than counting
samples. Brief scheduling delays therefore do not silently change how many
seconds an interval represents.

If meaningful activity resumes during Grace, the detector returns to Active.
If the grace period expires, it returns to Waiting and removes automatic
intent.

`Stop keeping awake after traffic drops` sets the quiet interval after qualified
traffic falls below the threshold. When it ends, Chiù releases only its
automatic idle-sleep protection. It never puts the computer to sleep; the
operating system and any other wake reason determine whether and when it sleeps.

## Default settings

Chiù uses:

- meaningful receive rate: **1 MiB/s**
- continuous qualification time: **5 seconds**
- quiet grace period: **2 minutes**

These values tune the detector; they do not guarantee that every transfer above
1 MiB/s is a download or that every download will remain above that rate.

The `Download detection` submenu offers these tuning choices:

| Setting | Choices |
| --- | --- |
| Meaningful receive rate | 256 KiB/s, 1 MiB/s, 5 MiB/s |
| Stop keeping awake after traffic drops | 30 seconds, 2 minutes, 5 minutes, 15 minutes, 30 minutes |

The five-second qualification duration is not user-tunable. A restored setting
outside the listed menu presets is shown as a custom value rather than
silently replaced.

## Power eligibility

A qualified detector produces intent, but automatic protection is eligible
only when:

- `Keep awake during downloads` is enabled; and
- external power is present, or `Keep awake for downloads on battery` is
  enabled.

An unknown power source is treated conservatively like battery or limited
power. Changing the power source or either setting reconciles existing
qualified intent immediately; it does not restart qualification.

Battery eligibility applies only to the automatic reason. A manual keep-awake
session can still be started independently.

## Continuity and topology changes

Received counters are meaningful only while Chiù knows that samples belong to
one continuous source history. A counter reset, stale sample, lost source, or
interface-topology change clears prior qualification, grace, and automatic
intent. This prevents old counters from being interpreted as a sudden large
transfer.

Monitoring can resume when a valid source is available again, but activity
must qualify from a clean history.

## Expected trade-offs

Aggregate network activity cannot reveal intent:

- sustained streaming, synchronization, or other receive traffic may look like
  a download;
- a slow or bursty download may not qualify, or may rely on the grace period
  between bursts;
- traffic from multiple applications is combined;
- Chiù cannot name the application, remote host, URL, or downloaded file.

Lowering the threshold makes activation easier and can increase false
positives. Raising it can reduce false positives but miss slower transfers. A
longer grace period tolerates longer quiet gaps but holds protection longer
after activity stops.

See [How Chiù works](how-it-works.md) for the relationship between detector
intent, battery policy, and native protection, or [Troubleshooting](troubleshooting.md)
when automatic protection does not start as expected.
