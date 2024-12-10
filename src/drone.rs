use crossbeam_channel::{select_biased, Receiver, Sender};
use log::error;
use rand::random;
use std::collections::BTreeSet;
use std::collections::HashMap;
use wg_2024::controller::{DroneCommand, DroneEvent};
use wg_2024::drone::Drone;
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{
    Ack, FloodRequest, FloodResponse, Fragment, Nack, NackType, NodeType, Packet, PacketType,
};

pub struct RustRoveri {
    id: NodeId,
    controller_send: Sender<DroneEvent>,
    controller_recv: Receiver<DroneCommand>,
    packet_send: HashMap<NodeId, Sender<Packet>>,
    packet_recv: Receiver<Packet>,
    pdr: f32,
    should_terminate: bool,
    flood_ids: BTreeSet<(NodeId, u64)>,
}

impl Drone for RustRoveri {
    fn new(
        id: NodeId,
        controller_send: Sender<DroneEvent>,
        controller_recv: Receiver<DroneCommand>,
        packet_recv: Receiver<Packet>,
        packet_send: HashMap<NodeId, Sender<Packet>>,
        pdr: f32,
    ) -> Self {
        Self {
            id,
            controller_send,
            controller_recv,
            packet_send,
            packet_recv,
            pdr: pdr.clamp(0.0, 1.0),
            should_terminate: false,
            flood_ids: BTreeSet::new(),
        }
    }

    fn run(&mut self) {
        while !self.should_terminate {
            select_biased! {
                recv(self.controller_recv) -> command => {
                    if let Ok(command) = command {
                        self.handle_command(command);
                    }
                }
                recv(self.packet_recv) -> packet => {
                    if let Ok(packet) = packet {
                        self.handle_packet(packet);
                    }
                },
            }
        }
    }
}

impl RustRoveri {
    fn handle_command(&mut self, command: DroneCommand) {
        match command {
            DroneCommand::SetPacketDropRate(pdr) => self.set_packet_drop_rate(pdr),
            DroneCommand::AddSender(node_id, sender) => self.add_sender(node_id, sender),
            DroneCommand::Crash => self.crash(),
            DroneCommand::RemoveSender(node_id) => self.remove_sender(node_id),
        }
    }

    fn set_packet_drop_rate(&mut self, pdr: f32) {
        if (0.0..=1.0).contains(&pdr) {
            println!("Packet drop rate set to {}", pdr);
            self.pdr = pdr;
        } else {
            println!(
                "Tried to set packet drop rate to {} which is not inside the allowed bounds",
                pdr
            );
        }
    }

    fn add_sender(&mut self, node_id: NodeId, sender: Sender<Packet>) {
        match self.packet_send.insert(node_id, sender) {
            Some(_) => println!("Neighbor {} updated", node_id),
            None => println!("Neighbor {} added", node_id),
        }
    }

    fn crash(&mut self) {
        println!("Crashing drone {}...", self.id);
        self.should_terminate = true;
    }

    fn remove_sender(&mut self, node_id: NodeId) {
        match self.packet_send.remove(&node_id) {
            Some(_) => {
                println!("Removed node {} from neighbors", node_id);
            }
            None => {
                println!(
                    "Tried to remove node {} from neighbors, but no such node is present",
                    node_id,
                );
            }
        }
    }

    fn handle_packet(&mut self, packet: Packet) {
        match packet.pack_type {
            PacketType::MsgFragment(fragment) => {
                self.handle_fragment(fragment, packet.routing_header, packet.session_id);
            }
            PacketType::FloodRequest(req) => {
                self.handle_flood_request(req, packet.routing_header, packet.session_id);
            }
            PacketType::FloodResponse(res) => {
                self.handle_flood_response(res, packet.routing_header, packet.session_id);
            }
            PacketType::Ack(ack) => {
                self.handle_ack(ack, packet.routing_header, packet.session_id);
            }
            PacketType::Nack(nack) => {
                self.handle_nack(nack, packet.routing_header, packet.session_id);
            }
        }
    }

    fn handle_fragment(&self, fragment: Fragment, header: SourceRoutingHeader, session_id: u64) {
        if let Err(nack) = self.check_fragment(&header, &fragment) {
            self.send_nack(nack, &header, session_id);
            self.send_drop_event(PacketType::MsgFragment(fragment), header, session_id);
        } else {
            let msg_header = SourceRoutingHeader {
                hop_index: header.hop_index + 1,
                ..header
            };

            let msg_packet = Packet {
                pack_type: PacketType::MsgFragment(fragment),
                session_id,
                routing_header: msg_header,
            };

            self.send_packet(msg_packet);
        }
    }

    fn handle_flood_request(
        &mut self,
        req: FloodRequest,
        header: SourceRoutingHeader,
        session_id: u64,
    ) {
        // If the flood ID has already been received or, if the drone has no neighbors:
        // - The drone adds itself to the path_trace
        // - The drone creates a FloodResponse and sends it back
        // Otherwise
        // - The drone forwards the flood request to neighbors except the sender of the flood
        // request
        if let Some(&(previous, _)) = req.path_trace.last() {
            if self.flood_ids.insert((previous, req.flood_id)) {
                // Flood request not yet received
                // Get adjacent nodes (but the node that sent us the request)
                let adjacents = self.get_adjacents_but(previous);

                if adjacents.is_empty() {
                    self.begin_flood_response(req, session_id);
                } else {
                    // Forward packet to neighbors
                    let flood_req = FloodRequest {
                        flood_id: req.flood_id,
                        initiator_id: req.initiator_id,
                        path_trace: {
                            let mut trace = req.path_trace;
                            trace.push((self.id, NodeType::Drone));
                            trace
                        },
                    };

                    let flood_req_packet = Packet {
                        routing_header: header,
                        pack_type: PacketType::FloodRequest(flood_req),
                        session_id,
                    };

                    for sender in adjacents {
                        self.send_packet_to_sender(flood_req_packet.clone(), sender);
                    }
                }
            } else {
                // Flood request already received
                self.begin_flood_response(req, session_id);
            }
        } else {
            let message = format!(
                "{} The flood request path trace is empty",
                self.get_prefix()
            );
            error!("{}", message);
            panic!("{}", message);
        }
    }

    fn begin_flood_response(&self, req: FloodRequest, session_id: u64) {
        let flood_res = FloodResponse {
            flood_id: req.flood_id,
            path_trace: {
                let mut trace = req.path_trace;
                trace.push((self.id, NodeType::Drone));
                trace
            },
        };

        let flood_res_packet = Packet {
            routing_header: SourceRoutingHeader {
                hop_index: 1,
                hops: flood_res
                    .path_trace
                    .iter()
                    .map(|(id, _)| *id)
                    .rev()
                    .collect(),
            },
            pack_type: PacketType::FloodResponse(flood_res),
            session_id,
        };

        match flood_res_packet.routing_header.hops.get(1) {
            Some(next_id) if self.packet_send.contains_key(next_id) => {
                self.send_packet(flood_res_packet);
            }
            Some(_) => self.send_drone_event(DroneEvent::ControllerShortcut(flood_res_packet)),
            None => {
                let message = format!("{} Could not build FloodResponse", self.get_prefix());
                error!("{}", message);
                panic!("{}", message);
            }
        }
    }

    fn handle_flood_response(
        &self,
        res: FloodResponse,
        header: SourceRoutingHeader,
        session_id: u64,
    ) {
        if let Err(nack) = self.check_header(&header, 0) {
            self.send_nack(nack, &header, session_id);
            self.send_drop_event(PacketType::FloodResponse(res), header, session_id);
        } else {
            let next_index = header.hop_index + 1;
            let flood_res_packet = Packet {
                pack_type: PacketType::FloodResponse(res),
                routing_header: SourceRoutingHeader {
                    hops: header.hops,
                    hop_index: next_index,
                },
                session_id,
            };

            match flood_res_packet.routing_header.hops.get(next_index) {
                Some(next_id) if self.packet_send.contains_key(next_id) => {
                    self.send_packet(flood_res_packet);
                }
                Some(_) => self.send_drone_event(DroneEvent::ControllerShortcut(flood_res_packet)),
                None => {
                    let message = format!("{} Could not build FloodResponse", self.get_prefix());
                    error!("{}", message);
                    panic!("{}", message);
                }
            }
        }
    }

    fn handle_ack(&self, ack: Ack, header: SourceRoutingHeader, session_id: u64) {
        if self.check_header(&header, 0).is_err() {
            if header.hops.is_empty() {
                let message = format!("{} Could not build Ack", self.get_prefix());
                error!("{}", message);
                panic!("{}", message);
            } else {
                let ack_packet = Packet {
                    pack_type: PacketType::Ack(ack),
                    routing_header: header,
                    session_id,
                };
                self.send_drone_event(DroneEvent::ControllerShortcut(ack_packet));
            }
        } else {
            let ack_packet = Packet {
                pack_type: PacketType::Ack(ack),
                routing_header: SourceRoutingHeader {
                    hops: header.hops,
                    hop_index: header.hop_index + 1,
                },
                session_id,
            };

            // Forward packet
            self.send_packet(ack_packet);
        }
    }

    fn handle_nack(&self, nack: Nack, header: SourceRoutingHeader, session_id: u64) {
        if self.check_header(&header, 0).is_err() {
            if header.hops.is_empty() {
                let message = format!(
                    "{} Could not build Nack (Nack header is empty)",
                    self.get_prefix()
                );
                error!("{}", message);
                panic!("{}", message);
            } else {
                let nack_packet = Packet {
                    pack_type: PacketType::Nack(nack),
                    routing_header: header,
                    session_id,
                };
                self.send_drone_event(DroneEvent::ControllerShortcut(nack_packet));
            }
        } else {
            let nack_packet = Packet {
                pack_type: PacketType::Nack(nack),
                routing_header: SourceRoutingHeader {
                    hops: header.hops,
                    hop_index: header.hop_index + 1,
                },
                session_id,
            };

            // Forward packet
            self.send_packet(nack_packet);
        }
    }

    fn check_fragment(
        &self,
        header: &SourceRoutingHeader,
        fragment: &Fragment,
    ) -> Result<(), Nack> {
        let fragment_index = fragment.fragment_index;
        self.check_header(header, fragment_index)?;

        // Step 5: Determine whether to drop the packet based on the drone's Packet Drop Rate (PDR)
        if random::<f32>() <= self.pdr {
            let nack = Nack {
                fragment_index,
                nack_type: NackType::Dropped,
            };
            Err(nack)
        } else {
            Ok(())
        }
    }

    fn check_header(&self, header: &SourceRoutingHeader, fragment_index: u64) -> Result<(), Nack> {
        // Step 1: Check if hops[hop_index] matches the drone's own NodeId
        match header.hops.get(header.hop_index) {
            Some(&id) if id != self.id => {
                let nack = Nack {
                    fragment_index,
                    nack_type: NackType::UnexpectedRecipient(id),
                };
                return Err(nack);
            }
            Some(_) => {}
            None => {
                let nack = Nack {
                    fragment_index,
                    nack_type: NackType::ErrorInRouting(self.id),
                };
                return Err(nack);
            }
        }

        // Step 3: Determine if the drone is the final destination
        // Step 4: If next_hop is not a neighbor of the drone, send a Nack with ErrorInRouting
        // (including the problematic NodeId of next_hop) and terminate processing
        match header.hops.get(header.hop_index + 1) {
            Some(next_id) if !self.packet_send.contains_key(next_id) => {
                let nack = Nack {
                    fragment_index,
                    nack_type: NackType::ErrorInRouting(*next_id),
                };
                return Err(nack);
            }
            Some(_) => {}
            None => {
                let nack = Nack {
                    fragment_index,
                    nack_type: NackType::DestinationIsDrone,
                };
                return Err(nack);
            }
        }

        // Step 5: Proceed based on the packet type
        Ok(())
    }

    fn send_nack(&self, nack: Nack, header: &SourceRoutingHeader, session_id: u64) {
        if let Some(hops) = RustRoveri::reverse_hops(header) {
            // We are able reverse the hops vector and send the Nack
            let nack_packet = Packet {
                pack_type: PacketType::Nack(nack),
                routing_header: SourceRoutingHeader { hop_index: 1, hops },
                session_id,
            };
            match nack_packet.routing_header.hops.get(1) {
                Some(next_id) if self.packet_send.contains_key(next_id) => {
                    self.send_packet(nack_packet)
                }
                Some(_) => self.send_drone_event(DroneEvent::ControllerShortcut(nack_packet)),
                None => {
                    let message = format!(
                        "{} Could not build Nack (could not get previous hop)",
                        self.get_prefix()
                    );
                    error!("{}", message);
                    panic!("{}", message);
                }
            }
        } else if let Some(&first) = header.hops.first() {
            // We are NOT able reverse the hops vector
            // therefore we send a ControllerShortcut to the controller
            let nack_packet = Packet {
                pack_type: PacketType::Nack(nack),
                routing_header: SourceRoutingHeader {
                    hop_index: 1,
                    hops: vec![first],
                },
                session_id,
            };
            self.send_drone_event(DroneEvent::ControllerShortcut(nack_packet));
        } else {
            let message = format!(
                "{} Could not build Nack (could not get packet sender)",
                self.get_prefix()
            );
            error!("{}", message);
            panic!("{}", message);
        }
    }

    fn get_sender(&self, packet: &Packet) -> Option<&Sender<Packet>> {
        let next_index = packet.routing_header.hop_index;
        let next_id = packet.routing_header.hops.get(next_index)?;
        self.packet_send.get(next_id)
    }

    fn send_packet(&self, packet: Packet) {
        match self.get_sender(&packet) {
            Some(sender) => {
                self.send_packet_to_sender(packet, sender);
            }
            None => {
                let message = format!("{} Could not reach neighbor", self.get_prefix());
                error!("{}", message);
                panic!("{}", message);
            }
        }
    }

    fn send_packet_to_sender(&self, packet: Packet, sender: &Sender<Packet>) {
        if sender.send(packet.clone()).is_ok() {
            self.send_drone_event(DroneEvent::PacketSent(packet));
        } else {
            let message = format!("{} The drone is disconnected", self.get_prefix());
            error!("{}", message);
            panic!("{}", message);
        }
    }

    fn send_drone_event(&self, drone_event: DroneEvent) {
        if self.controller_send.send(drone_event).is_err() {
            let message = format!("{} The controller is disconnected", self.get_prefix());
            error!("{}", message);
            panic!("{}", message);
        }
    }

    fn send_drop_event(
        &self,
        pack_type: PacketType,
        routing_header: SourceRoutingHeader,
        session_id: u64,
    ) {
        self.send_drone_event(DroneEvent::PacketDropped(Packet {
            pack_type,
            routing_header,
            session_id,
        }));
    }

    fn get_adjacents_but(&self, node_id: NodeId) -> Vec<&Sender<Packet>> {
        self.packet_send
            .iter()
            .filter(|(&id, _)| id != node_id)
            .map(|(_, sender)| sender)
            .collect()
    }

    fn reverse_hops(header: &SourceRoutingHeader) -> Option<Vec<u8>> {
        Some(
            header
                .hops
                .get(..header.hop_index + 1)?
                .iter()
                .cloned()
                .rev()
                .collect::<Vec<u8>>(),
        )
    }

    fn get_prefix(&self) -> String {
        format!("[DRONE {}]", self.id)
    }
}

#[cfg(test)]
mod drone_test {
    use crate::drone::DroneCommand;
    use crate::drone::DroneEvent;
    use crate::drone::FloodRequest;
    use crate::drone::HashMap;
    use crate::drone::Nack;
    use crate::drone::NackType;
    use crate::drone::NodeId;
    use crate::drone::NodeType;
    use crate::drone::Packet;
    use crate::drone::PacketType;
    use crate::drone::SourceRoutingHeader;
    use crate::RustRoveri;
    use crossbeam_channel::unbounded;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;
    use wg_2024::drone::Drone;
    use wg_2024::packet::Fragment;
    use wg_2024::packet::FRAGMENT_DSIZE;
    use wg_2024::tests::*;

    const TIMEOUT: Duration = Duration::from_millis(400);

    #[test]
    fn test_initialization_1() {
        // Set parameters
        const DRONE_ID: NodeId = 0;
        const PDR: f32 = 0.07;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (_controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        let drone = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            PDR,
        );

        assert_eq!(drone.id, DRONE_ID, "Drone id is not set");
        assert_eq!(drone.pdr, 0.07, "Drone pdr is not set");
        assert!(drone.packet_send.is_empty(), "packet_set is not empty");
        assert!(!drone.should_terminate, "should_terminate is already true");
        assert!(drone.flood_ids.is_empty(), "flood_ids is not empty");
    }

    #[test]
    fn test_initialization_2() {
        // Set parameters
        const DRONE_ID: NodeId = 0;
        const PDR_1: f32 = 1.1;
        const PDR_2: f32 = -0.1;

        // Create drone 1
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (_controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        let drone_1 = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            PDR_1,
        );

        // Create drone 2
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (_controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        let drone_2 = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            PDR_2,
        );

        assert_eq!(drone_1.pdr, 1.0, "Drone pdr is not 1.0");
        assert_eq!(drone_2.pdr, 0.0, "Drone pdr is not 0.0");
    }

    #[test]
    fn test_set_packet_drop_rate_1() {
        // Set parameters
        const DRONE_ID: NodeId = 69;
        const INITIAL_PDR: f32 = 0.3;
        const INVALID_PDR: f32 = -0.1;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let drone_outer = Arc::new(Mutex::new(RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            INITIAL_PDR,
        )));
        let drone_inner = drone_outer.clone();
        let handle = thread::spawn(move || {
            drone_inner
                .lock()
                .expect("Drone locked inside drone thread")
                .run()
        });

        // Change packet drop rate
        let _ = controller_recv_tx.send(DroneCommand::SetPacketDropRate(INVALID_PDR));

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let pdr = drone_outer
            .lock()
            .expect("Drone locked inside main thread")
            .pdr;
        assert_eq!(pdr, INITIAL_PDR, "Pdr changed");
        assert_ne!(pdr, INVALID_PDR, "Pdr changed");
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_set_packet_drop_rate_2() {
        // Set parameters
        const DRONE_ID: NodeId = 69;
        const INITIAL_PDR: f32 = 0.3;
        const INVALID_PDR: f32 = 1.1;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let drone_outer = Arc::new(Mutex::new(RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            INITIAL_PDR,
        )));
        let drone_inner = drone_outer.clone();
        let handle = thread::spawn(move || {
            drone_inner
                .lock()
                .expect("Drone locked inside drone thread")
                .run()
        });

        // Change packet drop rate
        let _ = controller_recv_tx.send(DroneCommand::SetPacketDropRate(INVALID_PDR));

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let pdr = drone_outer
            .lock()
            .expect("Drone locked inside main thread")
            .pdr;
        assert_eq!(pdr, INITIAL_PDR, "Pdr changed");
        assert_ne!(pdr, INVALID_PDR, "Pdr changed");
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_set_packet_drop_rate_3() {
        // Set parameters
        const DRONE_ID: NodeId = 69;
        const INITIAL_PDR: f32 = 0.3;
        const VALID_PDR: f32 = 0.5;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let drone_outer = Arc::new(Mutex::new(RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            INITIAL_PDR,
        )));
        let drone_inner = drone_outer.clone();
        let handle = thread::spawn(move || {
            drone_inner
                .lock()
                .expect("Drone locked inside drone thread")
                .run()
        });

        // Change packet drop rate
        let _ = controller_recv_tx.send(DroneCommand::SetPacketDropRate(VALID_PDR));

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let pdr = drone_outer
            .lock()
            .expect("Drone locked inside main thread")
            .pdr;
        assert_eq!(pdr, VALID_PDR, "Pdr is unchanged");
        assert_ne!(pdr, INITIAL_PDR, "Pdr is unchanged");
        assert!(handle.join().is_ok(), "Drone didn't panick");
    }

    #[test]
    fn test_crash() {
        // Set parameters
        const DRONE_ID: NodeId = 69;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let drone_outer = Arc::new(Mutex::new(RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.3,
        )));
        let drone_inner = drone_outer.clone();
        let handle = thread::spawn(move || {
            drone_inner
                .lock()
                .expect("Drone locked inside drone thread")
                .run()
        });

        // Crash the drone
        let _ = controller_recv_tx.send(DroneCommand::Crash);
        thread::sleep(Duration::from_millis(100));

        // Assert
        let should_terminate = drone_outer
            .lock()
            .expect("Drone locked inside main thread")
            .should_terminate;
        assert!(should_terminate, "Drone did not set should_terminate");
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_flood_request_empty() {
        // Set parameters
        const DRONE_ID: NodeId = 69;
        const INITIATOR_ID: NodeId = 0;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let mut drone = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.3,
        );
        let handle = thread::spawn(move || drone.run());

        // Send flood requests
        for i in (0..200).rev() {
            let packet = Packet {
                pack_type: PacketType::FloodRequest(FloodRequest {
                    flood_id: i,
                    initiator_id: INITIATOR_ID,
                    path_trace: Vec::new(),
                }),
                routing_header: SourceRoutingHeader {
                    hops: Vec::new(),
                    hop_index: 0,
                },
                session_id: 0,
            };
            let _ = packet_send_tx.send(packet);
        }

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        assert!(handle.join().is_err(), "Drone didn't panic");
    }

    #[test]
    fn test_add_sender() {
        // Set parameters
        const DRONE_ID: NodeId = 69;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let drone_outer = Arc::new(Mutex::new(RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.3,
        )));
        let drone_inner = drone_outer.clone();
        let handle = thread::spawn(move || {
            drone_inner
                .lock()
                .expect("Drone locked inside drone thread")
                .run()
        });

        // Add sender (neighbor)
        let (tx, _rx) = unbounded::<Packet>();
        let _ = controller_recv_tx.send(DroneCommand::AddSender(DRONE_ID, tx));

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let binding = drone_outer.lock().expect("Drone locked inside main thread");
        let sender = binding.packet_send.get(&DRONE_ID);
        assert!(sender.is_some(), "Sender wasn't added");
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_remove_sender() {
        // Set parameters
        const DRONE_ID: NodeId = 0;
        const SENDER_ID: NodeId = 20;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (_packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let drone_outer = Arc::new(Mutex::new(RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.3,
        )));
        let drone_inner = drone_outer.clone();
        let handle = thread::spawn(move || {
            drone_inner
                .lock()
                .expect("Drone locked inside drone thread")
                .run()
        });

        // Add and remove sender (neighbor)
        let (tx, _rx) = unbounded::<Packet>();
        let _ = controller_recv_tx.send(DroneCommand::AddSender(SENDER_ID, tx));
        let _ = controller_recv_tx.send(DroneCommand::RemoveSender(SENDER_ID));

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let binding = drone_outer.lock().expect("Drone locked inside main thread");
        let sender = binding.packet_send.get(&SENDER_ID);
        assert!(sender.is_none(), "Sender doesn't exist");
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_check_header_unexpected_recipient() {
        // Set parameters
        const DRONE_ID: NodeId = 0;
        const SENDER_ID: NodeId = 69;
        const WRONG_DRONE_ID: NodeId = 70;

        // Create channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let mut drone = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.0,
        );
        let handle = thread::spawn(move || drone.run());

        // Add sender
        let (tx, rx) = unbounded::<Packet>();
        let _ = controller_recv_tx.send(DroneCommand::AddSender(SENDER_ID, tx));

        // Send packet to drone
        let packet = Packet {
            pack_type: PacketType::MsgFragment(Fragment {
                data: [0; FRAGMENT_DSIZE],
                fragment_index: 0,
                length: 10,
                total_n_fragments: 1,
            }),
            routing_header: SourceRoutingHeader {
                hops: vec![SENDER_ID, WRONG_DRONE_ID],
                hop_index: 1,
            },
            session_id: 0,
        };
        let _ = packet_send_tx.send(packet);

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Get the Nack
        let nack_packet = rx
            .recv_timeout(TIMEOUT)
            .expect("Sender did not receive Nack");

        // Assert
        assert_eq!(
            nack_packet.routing_header.hops,
            vec![WRONG_DRONE_ID, SENDER_ID],
            "Nack routing header is not as expected",
        );
        match nack_packet.pack_type {
            PacketType::Nack(nack) => match nack.nack_type {
                NackType::UnexpectedRecipient(WRONG_DRONE_ID) => {}
                _ => panic!(
                    "Received Nack is not UnexpectedReceipient({})",
                    WRONG_DRONE_ID
                ),
            },
            _ => panic!("Received Packet is not a Nack"),
        }
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_check_header_error_in_routing_1() {
        // Set parameters
        const SENDER_ID: NodeId = 1;
        const DRONE_ID: NodeId = 2;
        const INVALID_HOP_INDEX: usize = 69420;

        // Create channels
        let (controller_send_tx, controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let mut drone = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.0,
        );
        let handle = thread::spawn(move || drone.run());

        // Add sender
        let (tx, _rx) = unbounded::<Packet>();
        controller_recv_tx
            .send(DroneCommand::AddSender(SENDER_ID, tx))
            .expect("Cant add dummy node sender to drone 1");

        // Send packet
        let packet = Packet {
            pack_type: PacketType::MsgFragment(Fragment {
                data: [0; FRAGMENT_DSIZE],
                fragment_index: 0,
                length: 10,
                total_n_fragments: 1,
            }),
            routing_header: SourceRoutingHeader {
                hops: vec![SENDER_ID, DRONE_ID],
                hop_index: INVALID_HOP_INDEX, // Index outside of bounds
            },
            session_id: 0,
        };
        let _ = packet_send_tx.send(packet);

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let drone_event = controller_send_rx
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive DroneEvent");
        match drone_event {
            DroneEvent::ControllerShortcut(packet) => match packet.pack_type {
                PacketType::Nack(nack) => match nack.nack_type {
                    NackType::ErrorInRouting(DRONE_ID) => {}
                    _ => panic!("Received Nack is not ErrorInRouting({})", DRONE_ID,),
                },
                _ => panic!("Received Packet is not a Nack"),
            },
            _ => panic!("Received DroneEvent is not ControllerShortcut"),
        }
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_check_header_error_in_routing_2() {
        // Set parameters
        const DRONE_ID: NodeId = 0;
        const SENDER_ID: NodeId = 69;
        const UNEXISTENT_ID: NodeId = 70;

        // Create drone channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let mut drone = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.0,
        );
        let handle = thread::spawn(move || drone.run());

        // Add sender
        let (tx, rx) = unbounded::<Packet>();
        controller_recv_tx
            .send(DroneCommand::AddSender(SENDER_ID, tx))
            .expect("Cant add dummy node sender to drone 1");

        // Send packet
        let packet = Packet {
            pack_type: PacketType::MsgFragment(Fragment {
                data: [0; FRAGMENT_DSIZE],
                fragment_index: 0,
                length: 10,
                total_n_fragments: 1,
            }),
            routing_header: SourceRoutingHeader {
                hops: vec![SENDER_ID, DRONE_ID, UNEXISTENT_ID],
                hop_index: 1,
            },
            session_id: 0,
        };
        packet_send_tx
            .send(packet)
            .expect("Cant send packet to drone 1");

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let nack_packet = rx
            .recv_timeout(TIMEOUT)
            .expect("Sender did not receive Nack");
        assert_eq!(
            nack_packet.routing_header.hops,
            vec![DRONE_ID, SENDER_ID],
            "Nack routing header is not as expected",
        );
        match nack_packet.pack_type {
            PacketType::Nack(nack) => match nack.nack_type {
                NackType::ErrorInRouting(UNEXISTENT_ID) => {}
                _ => panic!("Received Nack is not ErrorInRouting({})", UNEXISTENT_ID),
            },
            _ => panic!("Received Packet is not a Nack"),
        }
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_check_header_destination_is_drone() {
        // Set parameters
        const DRONE_ID: NodeId = 34;
        const SENDER_ID: NodeId = 35;

        // Create drone channels
        let (controller_send_tx, _controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let mut drone = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            0.0,
        );
        let handle = thread::spawn(move || drone.run());

        // Add sender
        let (tx, rx) = unbounded::<Packet>();
        controller_recv_tx
            .send(DroneCommand::AddSender(SENDER_ID, tx))
            .expect("Cant add dummy node sender to drone 1");

        // Send packet
        let packet = Packet {
            pack_type: PacketType::MsgFragment(Fragment {
                data: [0; FRAGMENT_DSIZE],
                fragment_index: 0,
                length: 10,
                total_n_fragments: 1,
            }),
            routing_header: SourceRoutingHeader {
                hops: vec![SENDER_ID, DRONE_ID], // Last hop is the drone itself
                hop_index: 1,
            },
            session_id: 0,
        };
        packet_send_tx
            .send(packet)
            .expect("Cant send packet to drone 1");

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let nack_packet = rx
            .recv_timeout(TIMEOUT)
            .expect("Dummy node did not received a packet");
        assert_eq!(
            nack_packet.routing_header.hops,
            vec![DRONE_ID, SENDER_ID],
            "Nack routing header is not as expected",
        );
        match nack_packet.pack_type {
            PacketType::Nack(nack) => match nack.nack_type {
                NackType::DestinationIsDrone => {}
                _ => panic!("Received Nack is not DestinationIsDrone"),
            },
            _ => panic!("Received Packet is not a Nack"),
        }
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_check_fragment_dropped() {
        // Set parameters
        const DRONE_ID: NodeId = 34;
        const SENDER_ID: NodeId = 35;

        // Create drone channels
        let (controller_send_tx, controller_send_rx) = unbounded::<DroneEvent>();
        let (controller_recv_tx, controller_recv_rx) = unbounded::<DroneCommand>();
        let (packet_send_tx, packet_recv_rx) = unbounded::<Packet>();
        let packet_send = HashMap::new();

        // Spawn the drone
        let mut drone = RustRoveri::new(
            DRONE_ID,
            controller_send_tx,
            controller_recv_rx,
            packet_recv_rx,
            packet_send,
            1.0,
        );
        let handle = thread::spawn(move || drone.run());

        // Add sender
        let (tx, rx) = unbounded::<Packet>();
        controller_recv_tx
            .send(DroneCommand::AddSender(SENDER_ID, tx))
            .expect("Cant add dummy node sender to drone 1");

        // Send packet
        let packet = Packet {
            pack_type: PacketType::MsgFragment(Fragment {
                data: [0; FRAGMENT_DSIZE],
                fragment_index: 0,
                length: 10,
                total_n_fragments: 1,
            }),
            routing_header: SourceRoutingHeader {
                hops: vec![SENDER_ID, DRONE_ID, SENDER_ID],
                hop_index: 1,
            },
            session_id: 0,
        };
        packet_send_tx
            .send(packet)
            .expect("Cant send packet to drone 1");

        // Crash the drone
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx.send(DroneCommand::Crash);

        // Assert
        let drone_event_1 = controller_send_rx
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketDropped");
        match drone_event_1 {
            DroneEvent::PacketSent(_) => {}
            _ => panic!("Received DroneEvent is not PacketSent"),
        }

        let drone_event_2 = controller_send_rx
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive Dropped");
        match drone_event_2 {
            DroneEvent::PacketDropped(_) => {}
            _ => panic!("Received DroneEvent is not Dropped"),
        }

        let nack_packet = rx
            .recv_timeout(TIMEOUT)
            .expect("Sender did not receive Nack");
        match nack_packet.pack_type {
            PacketType::Nack(nack) => match nack.nack_type {
                NackType::Dropped => {}
                _ => panic!("Received Nack is not Dropped"),
            },
            _ => panic!("Received Packet is not Nack"),
        }

        assert!(
            rx.recv_timeout(TIMEOUT).is_err(),
            "Receiver received dropped packet"
        );
        assert!(handle.join().is_ok(), "Drone panicked");
    }

    #[test]
    fn test_communication_integrity() {
        // Topology:
        // tx                       rx
        //   \                     /
        //    --- 1 --- 2 --- 3 ---

        // Set parameters
        const SENDER_ID: NodeId = 67;
        const DRONE_1_ID: NodeId = 68;
        const DRONE_2_ID: NodeId = 69;
        const DRONE_3_ID: NodeId = 70;
        const RECEIVER_ID: NodeId = 71;

        // Create sender channels
        let (sender_tx, _sender_rx) = unbounded::<Packet>();

        // Create receiver channels
        let (receiver_tx, receiver_rx) = unbounded::<Packet>();

        // Create drone 1 channels
        let (controller_1_send_tx, controller_1_send_rx) = unbounded::<DroneEvent>();
        let (controller_1_recv_tx, controller_1_recv_rx) = unbounded::<DroneCommand>();
        let (packet_1_send_tx, packet_1_recv_rx) = unbounded::<Packet>();
        let packet_send_1 = HashMap::new();

        // Spawn drone 1
        let mut drone_1 = RustRoveri::new(
            DRONE_1_ID,
            controller_1_send_tx,
            controller_1_recv_rx,
            packet_1_recv_rx,
            packet_send_1,
            0.0,
        );
        let handle_1 = thread::spawn(move || drone_1.run());

        // Create drone 2 channels
        let (controller_2_send_tx, controller_2_send_rx) = unbounded::<DroneEvent>();
        let (controller_2_recv_tx, controller_2_recv_rx) = unbounded::<DroneCommand>();
        let (packet_2_send_tx, packet_2_recv_rx) = unbounded::<Packet>();
        let packet_send_2 = HashMap::new();

        // Spawn drone 2
        let mut drone_2 = RustRoveri::new(
            DRONE_2_ID,
            controller_2_send_tx,
            controller_2_recv_rx,
            packet_2_recv_rx,
            packet_send_2,
            0.0,
        );
        let handle_2 = thread::spawn(move || drone_2.run());

        // Create drone 3 channels
        let (controller_3_send_tx, controller_3_send_rx) = unbounded::<DroneEvent>();
        let (controller_3_recv_tx, controller_3_recv_rx) = unbounded::<DroneCommand>();
        let (packet_3_send_tx, packet_3_recv_rx) = unbounded::<Packet>();
        let packet_send_3 = HashMap::new();

        // Spawn drone 3
        let mut drone_3 = RustRoveri::new(
            DRONE_3_ID,
            controller_3_send_tx,
            controller_3_recv_rx,
            packet_3_recv_rx,
            packet_send_3,
            0.0,
        );
        let handle_3 = thread::spawn(move || drone_3.run());

        // Add senders to simulate the network
        controller_1_recv_tx
            .send(DroneCommand::AddSender(SENDER_ID, sender_tx))
            .expect("Cant add sender to drone 1 neighbors");

        controller_1_recv_tx
            .send(DroneCommand::AddSender(
                DRONE_2_ID,
                packet_2_send_tx.clone(),
            ))
            .expect("Cant add drone 2 to drone 1 neighbors");

        controller_2_recv_tx
            .send(DroneCommand::AddSender(
                DRONE_1_ID,
                packet_1_send_tx.clone(),
            ))
            .expect("Cant add drone 1 to drone 2 neighbors");

        controller_2_recv_tx
            .send(DroneCommand::AddSender(DRONE_3_ID, packet_3_send_tx))
            .expect("Cant add drone 3 to drone 2 neighbors");

        controller_3_recv_tx
            .send(DroneCommand::AddSender(DRONE_2_ID, packet_2_send_tx))
            .expect("Cant add drone 2 to drone 3 neighbors");

        controller_3_recv_tx
            .send(DroneCommand::AddSender(RECEIVER_ID, receiver_tx))
            .expect("Cant add receiver to drone 3 neighbors");

        // Create fragment
        let fragment = Fragment {
            data: [69; FRAGMENT_DSIZE],
            fragment_index: 0,
            length: 10,
            total_n_fragments: 1,
        };

        // Create packet
        let packet = Packet {
            pack_type: PacketType::MsgFragment(fragment.clone()),
            routing_header: SourceRoutingHeader {
                hops: vec![SENDER_ID, DRONE_1_ID, DRONE_2_ID, DRONE_3_ID, RECEIVER_ID],
                hop_index: 1,
            },
            session_id: 0,
        };

        // Send packet to drone 1
        packet_1_send_tx
            .send(packet)
            .expect("Cant send packet to drone 1");

        // Crash the drones
        thread::sleep(Duration::from_millis(100));
        let _ = controller_1_recv_tx.send(DroneCommand::Crash);
        let _ = controller_2_recv_tx.send(DroneCommand::Crash);
        let _ = controller_3_recv_tx.send(DroneCommand::Crash);

        // Assert
        let assert_fragment = |packet: Packet| match packet.pack_type {
            PacketType::MsgFragment(received_fragment) => {
                assert_eq!(
                    received_fragment.fragment_index, fragment.fragment_index,
                    "Received fragment (fragment_index) is changed after forwarding"
                );
                assert_eq!(
                    received_fragment.total_n_fragments, fragment.total_n_fragments,
                    "Received fragment (total_n_fragments) is changed after forwarding"
                );
                assert_eq!(
                    received_fragment.length, fragment.length,
                    "Received fragment (length) is changed after forwarding"
                );
                assert_eq!(
                    received_fragment.data, fragment.data,
                    "Received fragment (data) is changed after forwarding"
                );
            }
            _ => panic!("Received Packet is not a Fragment"),
        };

        let assert_packet_sent = |drone_event: DroneEvent| match drone_event {
            DroneEvent::PacketSent(packet) => assert_fragment(packet),
            _ => panic!("Received DroneEvent is not a PacketSent"),
        };

        let drone_event_1 = controller_1_send_rx
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 1");
        assert_packet_sent(drone_event_1);

        let drone_event_2 = controller_2_send_rx
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 2");
        assert_packet_sent(drone_event_2);

        let drone_event_3 = controller_3_send_rx
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 3");
        assert_packet_sent(drone_event_3);

        let packet = receiver_rx
            .recv_timeout(TIMEOUT)
            .expect("Receiver did not receive a packet");
        assert_fragment(packet);

        assert!(handle_1.join().is_ok(), "Drone 1 panicked");
        assert!(handle_2.join().is_ok(), "Drone 2 panicked");
        assert!(handle_3.join().is_ok(), "Drone 3 panicked");
    }

    #[test]
    fn test_handle_flood_request_1() {
        // Topology:
        // rx/tx --- 1
        //            \
        //             --- 2

        // Set parameters
        const SENDER_ID: NodeId = 70;
        const DRONE_1_ID: NodeId = 71;
        const DRONE_2_ID: NodeId = 72;

        // Create receiver channels
        let (sender_recv_tx, sender_recv_rx) = unbounded::<Packet>();

        // Create drone 1 channels
        let (controller_send_tx_1, controller_send_rx_1) = unbounded::<DroneEvent>();
        let (controller_recv_tx_1, controller_recv_rx_1) = unbounded::<DroneCommand>();
        let (packet_recv_tx_1, packet_recv_rx_1) = unbounded::<Packet>();
        let packet_send_1 = HashMap::new();

        // Spawn drone 1
        let mut drone_1 = RustRoveri::new(
            DRONE_1_ID,
            controller_send_tx_1,
            controller_recv_rx_1,
            packet_recv_rx_1.clone(),
            packet_send_1,
            0.0,
        );
        let handle_1 = thread::spawn(move || drone_1.run());

        // Create drone 2 channels
        let (controller_send_tx_2, controller_send_rx_2) = unbounded::<DroneEvent>();
        let (controller_recv_tx_2, controller_recv_rx_2) = unbounded::<DroneCommand>();
        let (packet_recv_tx_2, packet_recv_rx_2) = unbounded::<Packet>();
        let packet_send_2 = HashMap::new();

        // Spawn drone 2
        let mut drone_2 = RustRoveri::new(
            DRONE_2_ID,
            controller_send_tx_2,
            controller_recv_rx_2,
            packet_recv_rx_2,
            packet_send_2,
            0.0,
        );
        let handle_2 = thread::spawn(move || drone_2.run());

        // Add senders to simulate the network
        // (sender <- 1)
        controller_recv_tx_1
            .send(DroneCommand::AddSender(SENDER_ID, sender_recv_tx))
            .expect("Cant add sender to drone 1 neighbors");

        // (1 -> 2)
        controller_recv_tx_1
            .send(DroneCommand::AddSender(DRONE_2_ID, packet_recv_tx_2))
            .expect("Cant add drone 2 to drone 1 neighbors");

        // (2 -> 1)
        controller_recv_tx_2
            .send(DroneCommand::AddSender(
                DRONE_1_ID,
                packet_recv_tx_1.clone(),
            ))
            .expect("Cant add drone 1 to drone 2 neighbors");

        // Create flood request
        let flood_request = FloodRequest {
            flood_id: 0,
            initiator_id: SENDER_ID,
            path_trace: vec![(SENDER_ID, NodeType::Client)],
        };

        // Create packet
        let packet = Packet {
            pack_type: PacketType::FloodRequest(flood_request.clone()),
            routing_header: SourceRoutingHeader {
                hops: vec![],
                hop_index: 0,
            },
            session_id: 0,
        };

        // Send packet to drone 1
        packet_recv_tx_1
            .send(packet)
            .expect("Cant send packet to drone 1");

        // Assert
        let assert_flood_request = |packet: Packet| match packet.pack_type {
            PacketType::FloodRequest(flood_request) => {
                assert_eq!(
                    flood_request.flood_id, flood_request.flood_id,
                    "Received flood request (flood_id) is changed after forwarding"
                );
            }
            _ => panic!("Received Packet is not a FloodRequest"),
        };
        let assert_flood_response = |packet: Packet| match packet.pack_type {
            PacketType::FloodResponse(flood_response) => {
                assert_eq!(
                    flood_request.flood_id, flood_response.flood_id,
                    "Received flood response (flood_id) is changed after forwarding"
                );
            }
            _ => panic!("Received Packet is not a FloodResponse"),
        };

        let drone_event_1 = controller_send_rx_1
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 1");
        match drone_event_1 {
            DroneEvent::PacketSent(packet) => assert_flood_request(packet),
            _ => panic!("Received DroneEvent is not a PacketSent"),
        };

        let drone_event_2 = controller_send_rx_2
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 2");
        match drone_event_2 {
            DroneEvent::PacketSent(packet) => assert_flood_response(packet),
            _ => panic!("Received DroneEvent is not a PacketSent"),
        };

        let packet = sender_recv_rx
            .recv_timeout(TIMEOUT)
            .expect("Sender did not receive a packet");
        assert_flood_response(packet);

        // Crash the drones
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx_1.send(DroneCommand::Crash);
        let _ = controller_recv_tx_2.send(DroneCommand::Crash);

        assert!(handle_1.join().is_ok(), "Drone 1 panicked");
        assert!(handle_2.join().is_ok(), "Drone 2 panicked");
    }

    #[test]
    fn test_handle_flood_request_2() {
        // Topology:
        //             --- 2
        //            /
        // rx/tx --- 1
        //            \
        //             --- 3

        // Set parameters
        const SENDER_ID: NodeId = 70;
        const DRONE_1_ID: NodeId = 71;
        const DRONE_2_ID: NodeId = 72;
        const DRONE_3_ID: NodeId = 73;

        // Create receiver channels
        let (sender_recv_tx, sender_recv_rx) = unbounded::<Packet>();

        // Create drone 1 channels
        let (controller_send_tx_1, controller_send_rx_1) = unbounded::<DroneEvent>();
        let (controller_recv_tx_1, controller_recv_rx_1) = unbounded::<DroneCommand>();
        let (packet_recv_tx_1, packet_recv_rx_1) = unbounded::<Packet>();
        let packet_send_1 = HashMap::new();

        // Spawn drone 1
        let mut drone_1 = RustRoveri::new(
            DRONE_1_ID,
            controller_send_tx_1,
            controller_recv_rx_1,
            packet_recv_rx_1.clone(),
            packet_send_1,
            0.0,
        );
        let handle_1 = thread::spawn(move || drone_1.run());

        // Create drone 2 channels
        let (controller_send_tx_2, controller_send_rx_2) = unbounded::<DroneEvent>();
        let (controller_recv_tx_2, controller_recv_rx_2) = unbounded::<DroneCommand>();
        let (packet_recv_tx_2, packet_recv_rx_2) = unbounded::<Packet>();
        let packet_send_2 = HashMap::new();

        // Spawn drone 2
        let mut drone_2 = RustRoveri::new(
            DRONE_2_ID,
            controller_send_tx_2,
            controller_recv_rx_2,
            packet_recv_rx_2,
            packet_send_2,
            0.0,
        );
        let handle_2 = thread::spawn(move || drone_2.run());

        // Create drone 3 channels
        let (controller_send_tx_3, controller_send_rx_3) = unbounded::<DroneEvent>();
        let (controller_recv_tx_3, controller_recv_rx_3) = unbounded::<DroneCommand>();
        let (packet_recv_tx_3, packet_recv_rx_3) = unbounded::<Packet>();
        let packet_send_3 = HashMap::new();

        // Spawn drone 3
        let mut drone_3 = RustRoveri::new(
            DRONE_3_ID,
            controller_send_tx_3,
            controller_recv_rx_3,
            packet_recv_rx_3,
            packet_send_3,
            0.0,
        );
        let handle_3 = thread::spawn(move || drone_3.run());

        // Add senders to simulate the network
        // (sender <- 1)
        controller_recv_tx_1
            .send(DroneCommand::AddSender(SENDER_ID, sender_recv_tx))
            .expect("Cant add sender to drone 1 neighbors");

        // (1 -> 2)
        controller_recv_tx_1
            .send(DroneCommand::AddSender(DRONE_2_ID, packet_recv_tx_2))
            .expect("Cant add drone 2 to drone 1 neighbors");

        // (2 -> 1)
        controller_recv_tx_2
            .send(DroneCommand::AddSender(
                DRONE_1_ID,
                packet_recv_tx_1.clone(),
            ))
            .expect("Cant add drone 1 to drone 2 neighbors");

        // (1 -> 3)
        controller_recv_tx_1
            .send(DroneCommand::AddSender(DRONE_3_ID, packet_recv_tx_3))
            .expect("Cant add drone 3 to drone 1 neighbors");

        // (3 -> 1)
        controller_recv_tx_3
            .send(DroneCommand::AddSender(
                DRONE_1_ID,
                packet_recv_tx_1.clone(),
            ))
            .expect("Cant add drone 1 to drone 3 neighbors");

        // Create flood request
        let flood_request = FloodRequest {
            flood_id: 0,
            initiator_id: SENDER_ID,
            path_trace: vec![(SENDER_ID, NodeType::Client)],
        };

        // Create packet
        let packet = Packet {
            pack_type: PacketType::FloodRequest(flood_request.clone()),
            routing_header: SourceRoutingHeader {
                hops: vec![],
                hop_index: 0,
            },
            session_id: 0,
        };

        // Send packet to drone 1
        packet_recv_tx_1
            .send(packet)
            .expect("Cant send packet to drone 1");

        // Assert
        let assert_flood_request = |packet: Packet| match packet.pack_type {
            PacketType::FloodRequest(flood_request) => {
                assert_eq!(
                    flood_request.flood_id, flood_request.flood_id,
                    "Received flood request (flood_id) is changed after forwarding"
                );
            }
            _ => panic!("Received Packet is not a FloodRequest"),
        };
        let assert_flood_response = |packet: Packet| match packet.pack_type {
            PacketType::FloodResponse(flood_response) => {
                assert_eq!(
                    flood_request.flood_id, flood_response.flood_id,
                    "Received flood response (flood_id) is changed after forwarding"
                );
            }
            _ => panic!("Received Packet is not a FloodResponse"),
        };

        let drone_event_1 = controller_send_rx_1
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 1");
        match drone_event_1 {
            DroneEvent::PacketSent(packet) => assert_flood_request(packet),
            _ => panic!("Received DroneEvent is not a PacketSent"),
        };

        let drone_event_2 = controller_send_rx_2
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 2");
        match drone_event_2 {
            DroneEvent::PacketSent(packet) => assert_flood_response(packet),
            _ => panic!("Received DroneEvent is not a PacketSent"),
        };

        let drone_event_3 = controller_send_rx_3
            .recv_timeout(TIMEOUT)
            .expect("Controller did not receive PacketSent event from drone 3");
        match drone_event_3 {
            DroneEvent::PacketSent(packet) => assert_flood_response(packet),
            _ => panic!("Received DroneEvent is not a PacketSent"),
        };

        let packet = sender_recv_rx
            .recv_timeout(TIMEOUT)
            .expect("Sender did not receive a packet");
        assert_flood_response(packet);
        let packet = sender_recv_rx
            .recv_timeout(TIMEOUT)
            .expect("Sender did not receive a packet");
        assert_flood_response(packet);

        // Crash the drones
        thread::sleep(Duration::from_millis(100));
        let _ = controller_recv_tx_1.send(DroneCommand::Crash);
        let _ = controller_recv_tx_2.send(DroneCommand::Crash);
        let _ = controller_recv_tx_3.send(DroneCommand::Crash);

        assert!(handle_1.join().is_ok(), "Drone 1 panicked");
        assert!(handle_2.join().is_ok(), "Drone 2 panicked");
        assert!(handle_3.join().is_ok(), "Drone 3 panicked");
    }

    #[test]
    fn test_wg2024_generic_fragment_forward() {
        generic_fragment_forward::<RustRoveri>()
    }

    #[test]
    fn test_wg2024_generic_fragment_drop() {
        // Client 1
        let (c_send, c_recv) = unbounded();
        // Drone 11
        let (d_send, d_recv) = unbounded();
        // Drone 12
        let (d2_send, _d2_recv) = unbounded();
        // SC commands
        let (_d_command_send, d_command_recv) = unbounded();
        let (d_event_send, d_event_recv) = unbounded();

        let mut drone = RustRoveri::new(
            11,
            d_event_send,
            d_command_recv,
            d_recv,
            HashMap::from([(12, d2_send.clone()), (1, c_send.clone())]),
            1.0,
        );

        // Spawn the drone's run method in a separate thread
        thread::spawn(move || {
            drone.run();
        });

        let msg = Packet::new_fragment(
            SourceRoutingHeader {
                hop_index: 1,
                hops: vec![1, 11, 12, 21],
            },
            1,
            Fragment {
                fragment_index: 1,
                total_n_fragments: 1,
                length: 128,
                data: [1; 128],
            },
        );

        // "Client" sends packet to the drone
        d_send.send(msg.clone()).unwrap();

        let nack_packet = Packet::new_nack(
            SourceRoutingHeader {
                hop_index: 1,
                hops: vec![11, 1],
            },
            1,
            Nack {
                fragment_index: 1,
                nack_type: NackType::Dropped,
            },
        );

        // Client listens for packet from the drone (Dropped Nack)
        assert_eq!(c_recv.recv_timeout(TIMEOUT).unwrap(), nack_packet);
        // ADDITIONAL
        // (Since our drone first sends the Nack packet, then
        // the PacketSent event and finally PacketDropped event)
        assert_eq!(
            d_event_recv.recv_timeout(TIMEOUT).unwrap(),
            DroneEvent::PacketSent(nack_packet)
        );
        // SC listen for event from the drone
        assert_eq!(
            d_event_recv.recv_timeout(TIMEOUT).unwrap(),
            DroneEvent::PacketDropped(msg)
        );
    }

    #[test]
    fn test_wg2024_generic_chain_fragment_drop() {
        generic_chain_fragment_drop::<RustRoveri>()
    }

    #[test]
    fn test_wg2024_generic_chain_fragment_ack() {
        generic_chain_fragment_ack::<RustRoveri>();
    }
}
