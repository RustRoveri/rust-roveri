use criterion::{criterion_group, criterion_main, Criterion};
use crossbeam_channel::{unbounded, Receiver, Sender};
use rust_roveri::RustRoveri;
use std::{collections::HashMap, hint::black_box};
use wg_2024::{
    controller::{DroneCommand, DroneEvent},
    drone::Drone,
    network::NodeId,
    packet::Packet,
};

fn bench_new(c: &mut Criterion) {
    let id: NodeId = 0;
    let pdr: f32 = 0.0;
    let (controller_send_tx, controller_send_rx) = unbounded::<DroneEvent>();
    let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
    let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
    let packet_send = HashMap::new();

    c.bench_function("bench new", |b| {
        b.iter(|| {
            RustRoveri::new(
                black_box(id),
                black_box(controller_send_tx.clone()),
                black_box(controller_recv_rx.clone()),
                black_box(packet_recv_rx.clone()),
                black_box(packet_send.clone()),
                black_box(pdr),
            )
        })
    });
}

//fn criterion_benchmark(c: &mut Criterion) {
//    c.bench_function("bench new", |b| b.iter(|| fibonacci(black_box(20))));
//}

criterion_group!(benches, bench_new);
criterion_main!(benches);
