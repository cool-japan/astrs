# astrs-recording

The `.arec` recording container for AstRS: writer, reader, merger and
seekable index.

Determinism as a feature: an execution is a file you can replay. Container
format v1, magic `ASTRSREC`: a header carrying the HLC epoch, the dataflow
id and the embedded manifest; zstd-framed (`oxiarc`) entry blocks of
`{node, output, hlc, meta, payload}`; a seekable index footer. `Writer`
streams entries with bounded memory and crash-tolerant durability;
`Reader` opens a finished file by its trailer, or (via `recover::scan`) a
truncated one by scanning; `merge::merge` k-way merges several recordings
by HLC into one. `Stats` answers `astrs bag info`-shaped questions from the
footer index alone, without touching a single payload. This is the shared
vocabulary `astrs-record-node`, `astrs-replay-node`, daemon-side `astrs
record start`, and `.arec ⇄ rosbag2` conversion (`astrs-rosbag`) all build
on.

## Example

```rust
use astrs_recording::{Entry, Reader, Writer, WriterOptions};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, DataflowId, Metadata, NodeId};

let path = std::env::temp_dir().join(format!("astrs-recording-readme-{}.arec", std::process::id()));

let mut writer = Writer::create(
    &path,
    WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::new(1_000, 0))
        .with_manifest_yaml("nodes:\n  - id: camera\n    path: ./camera\n"),
)?;
writer.append(Entry::new(
    NodeId::new("camera")?,
    DataId::new("frames")?,
    Metadata::new(HlcTimestamp::new(1_000, 1)),
    vec![1, 2, 3],
))?;
writer.finish()?;

let mut reader = Reader::open(&path)?;
let entries: Vec<Entry> = reader.iter_all().collect::<Result<_, _>>()?;
assert_eq!(entries[0].payload, vec![1, 2, 3]);
# std::fs::remove_file(&path).ok();
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the [crate documentation](https://docs.rs/astrs-recording) for the
recording and replay design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
