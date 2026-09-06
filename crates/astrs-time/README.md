# astrs-time

Time and causality primitives for AstRS.

A first-party hybrid logical clock (the `uhlc` replacement — this
workspace's dependency policy bans `uhlc` outright) plus the surrounding
time vocabulary
the rest of the stack shares: `HlcTimestamp` (a compact, totally-ordered
64-bit-physical/32-bit-logical timestamp with `serde`, `oxicode` and a
`Display`/`FromStr` string form), `HlcClock` (issues timestamps that stay
monotone even when the wall clock is not, via the HLC send/receive rules),
`Stamped<T>` (the universal event wrapper every AstRS merged event loop
uses), a `Clock` abstraction with a fully controllable `ManualClock` for
deterministic tests and `--deterministic` replay, checked-arithmetic
`Deadline`s, human duration parsing/formatting (`"250ms"`, `"1.5h"`), and
`TimerInterval` for the drift-free `astrs/timer/*` virtual sources.

## Example

```rust
use astrs_time::{HlcClock, ManualClock};

let clock = HlcClock::new(ManualClock::new(1_000));
let event = clock.stamp("camera-frame-042");
assert_eq!(event.inner, "camera-frame-042");

let next_event = clock.stamp("camera-frame-043");
assert!(next_event.ts > event.ts);
```

See the workspace [README](https://github.com/cool-japan/astrs#architecture)
for why every event in the system is `Stamped<T>`.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
