use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

fn fibonacci(n: u64) -> u64 {
    match n {
        0 => 1,
        1 => 1,
        n => fibonacci(n - 1) + fibonacci(n - 2),
    }
}

fn sum(n1: u32, n2: u32) -> u32 {
    n1 + n2
}

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("fib 20", |b| b.iter(|| fibonacci(black_box(20))));
    c.bench_function("sum", |b| b.iter(|| sum(black_box(20), black_box(20))));
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
