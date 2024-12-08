use criterion::{criterion_group, criterion_main, Criterion};
use crossbeam_channel::{unbounded, Receiver, Sender};
use rust_roveri::RustRoveri;
use std::{collections::HashMap, hint::black_box, thread};
use wg_2024::{
    controller::{DroneCommand, DroneEvent},
    drone::Drone,
    network::{NodeId, SourceRoutingHeader},
    packet::{Fragment, Packet, PacketType, FRAGMENT_DSIZE},
};

fn bench_new(c: &mut Criterion) {
    let id: NodeId = 0;
    let pdr: f32 = 0.0;
    let (controller_send_tx, controller_send_rx) = unbounded::<DroneEvent>();
    let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
    let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
    let packet_send = HashMap::new();

    c.bench_function("bench_new", |b| {
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

fn send_and_receive(packet: Packet, packet_send: Sender<Packet>, packet_recv: Receiver<Packet>) {
    let _ = packet_send.send(packet);
    let _ = packet_recv.recv();
}

fn bench_handle_fragment(c: &mut Criterion) {
    let DRONE_ID = 0;
    let RECEIVER_ID = 1;

    let pdr: f32 = 0.0;
    let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
    let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
    let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
    let packet_send = HashMap::new();

    // Create receiver channels
    let (receiver_tx, receiver_rx) = unbounded::<Packet>();

    let mut drone = RustRoveri::new(
        DRONE_ID,
        controller_send_tx,
        controller_recv_rx,
        packet_recv_rx,
        packet_send,
        pdr,
    );
    let handle = thread::spawn(move || drone.run());

    // Create packet
    let packet = Packet {
        pack_type: PacketType::MsgFragment(Fragment {
            data: [69; FRAGMENT_DSIZE],
            fragment_index: 0,
            length: 10,
            total_n_fragments: 1,
        }),
        routing_header: SourceRoutingHeader {
            hops: vec![DRONE_ID, RECEIVER_ID],
            hop_index: 0,
        },
        session_id: 0,
    };

    controller_recv_tx
        .send(DroneCommand::AddSender(RECEIVER_ID, receiver_tx))
        .expect("Cant add receiver to drone 1 neighbors");

    c.bench_function("bench new", |b| {
        b.iter(|| {
            send_and_receive(packet.clone(), packet_send_tx.clone(), receiver_rx.clone());
        })
    });

    let _ = handle.join();
}

//fn criterion_benchmark(c: &mut Criterion) {
//    c.bench_function("bench new", |b| b.iter(|| fibonacci(black_box(20))));
//}

criterion_group!(benches, bench_new, bench_handle_fragment);
criterion_main!(benches);
