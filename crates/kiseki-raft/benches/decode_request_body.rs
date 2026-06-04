// Formal local A/B for the `decode_request_body` lever.
//
// The flamegraph site (75.8 % of inter-node Raft RPC dispatch CPU)
// was postcard's per-byte `SeqAccess` deserialization of the request
// payload `Vec<u8>`. Wrapping the field as `serde_bytes::ByteBuf`
// triggers postcard's `deserialize_byte_buf` fast path — one bulk
// copy in place of N `visit_u8` calls.
//
// This bench decodes the same wire bytes both ways, side by side, at
// payload sizes that match the production hot path:
//   * 4 KiB   — object PUT (write_impl piece size floor)
//   * 64 KiB  — typical chunk + intent_put fan payload
//   * 1 MiB   — large AppendEntries batch upper bound
//
// Wire format is identical for `Vec<u8>` and `ByteBuf` in postcard
// (length-prefix + raw bytes), so this is a pure decoder A/B; the
// encoder is unchanged.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kiseki_common::ids::ShardId;
use uuid::Uuid;

const TAG: &str = "kiseki.raft.append_entries";
const SIZES: &[usize] = &[4 * 1024, 64 * 1024, 1024 * 1024];

/// Build the on-the-wire outer tuple bytes for a payload of `size`,
/// matching `encode_request_body`'s post-version-byte body.
fn outer_bytes(size: usize) -> Vec<u8> {
    let shard = ShardId(Uuid::from_u128(0x42));
    let payload: Vec<u8> = (0..size).map(|i| (i & 0xff) as u8).collect();
    let outer = (*shard.0.as_bytes(), TAG.to_owned(), payload);
    postcard::to_stdvec(&outer).expect("encode")
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_request_body");
    for &size in SIZES {
        let body = outer_bytes(size);
        group.throughput(Throughput::Bytes(body.len() as u64));

        // OLD: postcard decodes `Vec<u8>` via SeqAccess — one
        // `visit_u8` per byte. This is what the flamegraph showed
        // as 75.8 % of dispatch CPU.
        group.bench_with_input(BenchmarkId::new("vec_u8", size), &body, |b, body| {
            b.iter(|| {
                let (id, tag, payload): ([u8; 16], String, Vec<u8>) =
                    postcard::from_bytes(black_box(body)).expect("decode");
                black_box((id, tag, payload));
            });
        });

        // NEW: `serde_bytes::ByteBuf` calls `deserialize_byte_buf`,
        // which postcard satisfies with a single contiguous-byte
        // copy out of the input slice.
        group.bench_with_input(BenchmarkId::new("byte_buf", size), &body, |b, body| {
            b.iter(|| {
                let (id, tag, payload): ([u8; 16], String, serde_bytes::ByteBuf) =
                    postcard::from_bytes(black_box(body)).expect("decode");
                black_box((id, tag, payload));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
