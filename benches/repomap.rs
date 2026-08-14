use criterion::{criterion_group, criterion_main, Criterion};
use raven::repomap::{build_map, invalidate};
use std::path::Path;

fn make_workspace(dir: &Path, files: usize, lines_per_file: usize) {
    std::fs::create_dir_all(dir).unwrap();
    for f in 0..files {
        let mut body = String::new();
        for l in 0..lines_per_file {
            body.push_str(&format!(
                "pub fn function_{f}_{l}(x: i32) -> i32 {{ x * {l} }}\n"
            ));
            body.push_str(&format!("struct Struct_{f}_{l} {{}}\n"));
        }
        std::fs::write(dir.join(format!("mod_{f}.rs")), body).unwrap();
    }
}

fn bench(c: &mut Criterion) {
    let tmp = tempfile::tempdir().unwrap();
    make_workspace(tmp.path(), 60, 20);

    let mut g = c.benchmark_group("repomap");
    // Cold walk: drop the cache so each sample rescans. `build_map` caches
    // after the first call, so timing it without invalidate only measures
    // a HashMap hit.
    g.bench_function("build_map_cold_60_files", |b| {
        b.iter(|| {
            invalidate(tmp.path());
            build_map(std::hint::black_box(tmp.path()))
        })
    });
    let _ = build_map(tmp.path());
    g.bench_function("build_map_cached_60_files", |b| {
        b.iter(|| build_map(std::hint::black_box(tmp.path())))
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
