//! Network logic. This module includes:
//! * `Network`, a component assigned to entities
//! which can send and receive packets. (This is only
//! added for players, obviously.)
//! * `PacketQueue`, which stores packets received
//! from players and allows systems to poll for packets
//! received of a given type.

use crate::entity::{EntityDeleteEvent, EntityId};
use crate::io::{ListenerToServerMessage, NetworkIoManager, ServerToWorkerMessage};
use crate::player;
use crate::state::State;
use crossbeam::Receiver;
use feather_core::network::cast_packet;
use feather_core::{Packet, PacketType, Position};
use futures::channel::mpsc::UnboundedSender;
use legion::entity::Entity;
use legion::query::Read;
use lock_api::RawMutex;
use parking_lot::{Mutex, MutexGuard};
use std::iter;
use strum::EnumCount;
use tonks::{PreparedWorld, Query};
use uuid::Uuid;

type QueuedPackets = Vec<(Entity, Box<dyn Packet>)>;

struct UnsafeDrain<T> {
    ptr: *const T,
    len: usize,
    pos: usize,
}

impl<T> Iterator for UnsafeDrain<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos == self.len {
            return None;
        }

        let value = unsafe { std::ptr::read(self.ptr.add(self.pos)) };
        self.pos += 1;
        Some(value)
    }
}

pub struct DrainedPackets<'a, I> {
    mutex: &'a parking_lot::RawMutex,
    value: I,
}

impl<'a, I> DrainedPackets<'a, I> {
    unsafe fn new(mutex: &'a parking_lot::RawMutex, value: I) -> Self {
        Self { mutex, value }
    }
}

impl<'a, I> Iterator for DrainedPackets<'a, I>
where
    I: Iterator,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        self.value.next()
    }
}

impl<'a, I> Drop for DrainedPackets<'a, I> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

/// The packet queue. This type allows systems to poll for
/// received packets of a given type.
///
/// A system should never require mutable access to this type.
#[derive(Resource)]
pub struct PacketQueue {
    /// Vector of queued packets. This vector is indexed
    /// by the ordinal of the packet type, and each
    /// queue contains only packets of its type.
    queue: Vec<Mutex<QueuedPackets>>,
}

impl Default for PacketQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PacketQueue {
    /// Creates a new, empty `PacketQueue`.
    pub fn new() -> Self {
        Self {
            queue: iter::repeat_with(|| Mutex::new(vec![]))
                .take(PacketType::count() + 1)
                .collect(),
        }
    }

    /// Returns an iterator over packets of a given type.
    pub fn received<P: Packet>(&self) -> impl Iterator<Item = (Entity, P)> + '_ {
        let mut queue = self.queue[P::ty_sized().ordinal()].lock();

        // Hack to map to draining iterator.
        unsafe {
            let raw = MutexGuard::mutex(&queue).raw();

            let drain = UnsafeDrain {
                ptr: queue.as_ptr(),
                len: queue.len(),
                pos: 0,
            };

            // Safety: the vector cannot be accessed as long as the returned `UnsafeDrain`
            // has not been dropped, since the mutex is acquired.
            queue.set_len(0);
            // Ensure mutex is not released; we will do it manually in `UnsafeDrain`
            std::mem::forget(queue);

            let iter = drain.map(|(entity, packet)| (entity, cast_packet::<P>(packet)));

            DrainedPackets::new(raw, iter)
        }
    }

    /// Adds a packet to the queue.
    pub fn push(&self, packet: Box<dyn Packet>, entity: Entity) {
        let ordinal = packet.ty().ordinal();

        self.queue[ordinal].lock().push((entity, packet));
    }
}

/// Network component containing channels to send and receive packets.
///
/// Systems should call `Self::send` to send a packet to this entity (player).
pub struct Network {
    pub sender: UnboundedSender<ServerToWorkerMessage>,
    pub receiver: Receiver<ServerToWorkerMessage>,
}

impl Network {
    /// Sends a packet to this player.
    pub fn send<P>(&self, packet: P)
    where
        P: Packet,
    {
        self.send_boxed(Box::new(packet));
    }

    /// Sends a boxed packet to this player.
    pub fn send_boxed(&self, packet: Box<dyn Packet>) {
        // Discard error in case the channel was disconnected
        // (e.g. if the player disconnected and its worker task
        // shut down, and the disconnect was not yet registered
        // by the server)
        let _ = self
            .sender
            .unbounded_send(ServerToWorkerMessage::SendPacket(packet));
    }
}

/// The network system. This system is responsible for:
/// * Handling player disconnects.
/// * Pushing received packets to the packet queue.
/// * Accepting new clients and creating entities for them.
#[system]
pub fn network_(
    state: &State,
    io: &NetworkIoManager,
    packet_queue: &PacketQueue,
    query: &mut Query<Read<Network>>,
    world: &mut PreparedWorld,
) {
    // For each `Network`, handle any disconnects and received packets.
    query.par_entities_for_each(world, |(entity, network)| {
        while let Ok(msg) = network.receiver.try_recv() {
            match msg {
                ServerToWorkerMessage::NotifyDisconnect(_) => {
                    state.exec_with_scheduler(move |world, scheduler| {
                        let position = *world.get_component::<Position>(entity).unwrap();
                        let id = *world.get_component::<EntityId>(entity).unwrap();
                        let uuid = *world.get_component::<Uuid>(entity).unwrap();
                        scheduler.trigger(EntityDeleteEvent {
                            entity,
                            position: Some(position),
                            id,
                            uuid,
                        });
                        assert!(world.delete(entity), "player already deleted");
                    });
                }
                ServerToWorkerMessage::NotifyPacketReceived(packet) => {
                    packet_queue.push(packet, entity);
                }
                _ => unreachable!(),
            }
        }
    });

    // Handle new clients.
    while let Ok(msg) = io.receiver.try_recv() {
        match msg {
            ListenerToServerMessage::NewClient(info) => {
                debug!("Server received connection from {}", info.username);
                player::create(state, info);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feather_core::network::packet::implementation::Handshake;
    use feather_core::network::packet::PacketType::SpawnObject;
    use legion::world::World;

    #[test]
    fn packet_queue() {
        let queue = PacketQueue::new();

        let mut world = World::new();
        let entities = world.insert((), vec![(), ()]);

        queue.push(Box::new(Handshake::default()), entities[0]);
        queue.push(Box::new(SpawnObject::default()), entities[1]);
        queue.push(Box::new(Handshake::default()), entities[1]);

        let mut handshakes = queue.received::<Handshake>();
        assert_eq!(handshakes.next().unwrap().0, entities[0]);
        assert_eq!(handshakes.next().unwrap().0, entities[1]);
        assert!(handshakes.next().is_none());

        let mut spawn_objects = queue.received::<SpawnObject>();
        assert_eq!(spawn_objects.next().unwrap().0, entities[1]);
        assert!(spawn_objects.next().is_none());
    }
}
