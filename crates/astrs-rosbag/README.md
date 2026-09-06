# astrs-rosbag

rosbag2 `.db3` and `.mcap` reading, `.db3` writing and `.arec` conversion
for AstRS.

Existing robot data opens natively in AstRS: `.db3` reading and writing
through `oxisql-sqlite-compat`, so there is no C SQLite anywhere in the
build; `.mcap` reading, including the chunked and indexed layouts; and
conversion between rosbag2 files and the AstRS `.arec` container —
`.arec ⇄ .db3` both ways, `.mcap → .arec` one way (there is no `.mcap`
writer, so nothing converts back out to `.mcap`) — backing `astrs bag
convert` and `astrs bag info`. Format detection is centralized in one
place (`convert::detect_format`), so the CLI's `bag info`/`bag convert`
commands and this crate's own conversion path never disagree about what a
given file extension means.

See the [crate documentation](https://docs.rs/astrs-rosbag) for the rosbag2
and `.arec` conversion design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
