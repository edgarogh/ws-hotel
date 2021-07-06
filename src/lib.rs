//! # `ws-hotel`
//!
//! _Your websocket server, with rooms._

use std::any::Any;
use std::marker::PhantomData;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use ws::util::Token;
use ws::{CloseCode, Message, Sender};

/// A room in which websocket clients can be moved
///
/// It effectively contains a user-provided [`RoomHandler`] as R as well as a set of users that
/// are in the room.
pub struct Room<R: RoomHandler>(Arc<Mutex<RoomInner<R>>>);

struct RoomInner<R: RoomHandler> {
    handler: R,
    members: Vec<(R::Guest, Sender)>,
}

impl<R: RoomHandler> Room<R> {
    /// Constructs a new empty [Room]
    ///
    /// Clients can be moved inside using [`Context::relocate`]:
    ///
    /// ```no_run
    /// use ws_hotel::*;
    ///
    /// let room = AdHoc::new(|mut ctx: Context<String>, mut m: MembersAccess<String>, msg: Message| {
    ///     println!("message from: {}", m.identity());
    ///     ctx.broadcast(msg)
    /// });
    ///
    /// ws_hotel::listen(
    ///     "127.0.0.1:8080",
    ///     AdHoc::new(|mut ctx: Context<()>, _: MembersAccess<()>, msg: Message| {
    ///         // Let's assume that the first messager is the client introducing themselves
    ///         let name = msg.as_text().unwrap().clone();
    ///
    ///         ctx.relocate(&room, name);
    ///     }),
    /// );
    ///
    /// room.relocate(room, username);
    /// ```
    pub fn new(handler: R) -> Self {
        Self(Arc::new(Mutex::new(RoomInner {
            handler,
            members: Vec::new(),
        })))
    }

    /// Run code that needs access to the internal `RoomHandler`.
    ///
    /// Accessing it requires locking a Mutex, beware of deadlocks !
    #[inline]
    pub fn with<F: FnOnce(&mut R) -> T, T>(&self, f: F) -> T {
        f(&mut self.0.lock().unwrap().handler)
    }
}

type Relocation = Option<(Box<dyn Any>, Arc<dyn RoomAny>)>;

trait RoomAny {
    fn on_message(&self, sender: Sender, msg: Message) -> ws::Result<Relocation>;

    fn broadcast(&self, msg: Message) -> ws::Result<()>;
    fn add(&self, sender: Sender, identity: Box<dyn Any>);
    fn remove(&self, socket_id: (Token, u32));
}

impl<R: RoomHandler + 'static> RoomAny for Mutex<RoomInner<R>> {
    fn on_message(&self, sender: Sender, msg: Message) -> ws::Result<Relocation> {
        let mut room_guard = self.lock().unwrap();
        let room = &mut *room_guard;
        let handler = &mut room.handler;

        let mut relocation = None;

        // TODO clean up while merging
        let todo = room
            .members
            .iter()
            .map(|(_, v)| (PhantomData, v.clone()))
            .collect::<Vec<_>>();
        let socket = Context {
            sender: &sender,
            members: &todo,
            relocation: &mut relocation,
        };

        handler.on_message(
            socket,
            MembersAccess {
                members: &mut room.members,
                me: (sender.token(), sender.connection_id()),
            },
            msg,
        )?;

        Ok(relocation)
    }

    fn broadcast(&self, msg: Message) -> ws::Result<()> {
        self.lock()
            .unwrap()
            .members
            .iter()
            .map(|(_, sender)| sender.send(msg.clone()))
            .collect()
    }

    fn add(&self, sender: Sender, identity: Box<dyn Any>) {
        let identity = *identity.downcast().unwrap();
        self.lock().unwrap().members.push((identity, sender))
    }

    fn remove(&self, (token, connection_id): (Token, u32)) {
        self.lock().unwrap().members.retain(|(_, sender)| {
            !(sender.token() == token && sender.connection_id() == connection_id)
        });
    }
}

/// TODO temporary struct that should be merged with [Context]
pub struct MembersAccess<'a, Guest> {
    members: &'a mut [(Guest, Sender)],
    me: (Token, u32),
}

impl<Guest> MembersAccess<'_, Guest> {
    pub fn identity(&mut self) -> &mut Guest {
        // TODO memoize this function ? probably requires unsafe code

        let sender = self.me;

        &mut self
            .members
            .iter_mut()
            .find(move |(_, s)| (s.token(), s.connection_id()) == sender)
            .expect("guest not in room")
            .0
    }
}

pub struct Context<'a, Guest> {
    sender: &'a Sender,
    members: &'a [(PhantomData<Guest>, Sender)],
    relocation: &'a mut Relocation,
}

impl<Guest> Context<'_, Guest> {
    /// Sends a message to the client associated to this [Context], that is, the one who received
    /// the message.
    #[inline]
    pub fn send(&self, msg: impl Into<Message>) -> ws::Result<()> {
        self.sender.send(msg)
    }

    /// Sends a message to everyone in the same room
    pub fn broadcast(&self, msg: impl Into<Message>) -> ws::Result<()> {
        let msg = msg.into();

        self.members
            .iter()
            .map(|(_, sender)| sender.send(msg.clone()))
            .collect()
    }

    /// Moves the client to a new room
    pub fn relocate<R>(&mut self, room: &Room<R>, identity: R::Guest)
    where
        R: RoomHandler + 'static,
        R::Guest: Send + 'static,
    {
        let room1 = Arc::clone(&room.0);

        *self.relocation = Some((Box::new(identity) as _, room1 as _));
    }
}

struct Handler {
    sender: Sender,
    room: Arc<dyn RoomAny>,
}

impl ws::Handler for Handler {
    fn on_message(&mut self, msg: Message) -> ws::Result<()> {
        let relocation = self.room.on_message(self.sender.clone(), msg)?;

        if let Some((identity, room)) = relocation {
            self.room
                .remove((self.sender.token(), self.sender.connection_id()));
            self.room = room;
            self.room.add(self.sender.clone(), identity);
        }

        Ok(())
    }

    fn on_close(&mut self, _code: CloseCode, _reason: &str) {
        self.room
            .remove((self.sender.token(), self.sender.connection_id()));
    }
}

/// An event handler for a specific type of room.
///
/// # Guest
/// The associated [Guest] type represents an arbitrary mutable piece of data
/// that can be associated with each an every member of the room, depending on the use case.
///
/// [Guest]: RoomHandler::Guest
pub trait RoomHandler {
    /// Arbitrary piece of data that can be used to store room-kind-specific data for each member of
    /// the room.
    ///
    /// When moving someone into a new room, the caller of [`Context::relocate`] must pass an
    /// initial value. Whenever a message is received, the [`RoomHandler`] implementation will
    /// receive a mutable reference to this same value.
    ///
    /// For a chatroom, [Guest] can be set to [String] and can represent the name of a chat room
    /// member. For a card game, it can contain the deck of cards that will be mutated during the
    /// game.
    ///
    /// [Guest]: RoomHandler::Guest
    type Guest;

    fn on_message(
        &mut self,
        socket: Context<Self::Guest>,
        m: MembersAccess<Self::Guest>,
        msg: Message,
    ) -> ws::Result<()>;
}

/// A simple [RoomHandler] that wraps a function or closure that will be called when receiving a
/// message.
///
/// # Example
///
/// ```
/// // TODO when API is stable
/// assert_eq!(1 + 1, 2);
/// ```
pub struct AdHoc<F, G>(F, PhantomData<G>);

impl<F, G> AdHoc<F, G> {
    pub fn new(f: F) -> Self {
        Self(f, PhantomData)
    }
}

impl<Guest, F: FnMut(Context<Guest>, MembersAccess<Guest>, Message) -> ws::Result<()>> RoomHandler
    for AdHoc<F, Guest>
{
    type Guest = Guest;

    fn on_message(
        &mut self,
        socket: Context<Self::Guest>,
        m: MembersAccess<Self::Guest>,
        msg: Message,
    ) -> ws::Result<()> {
        self.0(socket, m, msg)
    }
}

/// Starts a WebSocket hotel given an address and a default room.
/// This function blocks indefinitely.
///
/// The default room is where clients will be put when connecting the server. Its associated
/// [`RoomHandler::Guest`] type must implement [`Default`], so it can be built implicitly.
pub fn listen<A, R>(addr: A, lobby: R)
where
    A: ToSocketAddrs + std::fmt::Debug,
    R: RoomHandler + 'static,
    R::Guest: Default + 'static,
{
    let lobby = Room(Arc::new(Mutex::new(RoomInner {
        handler: lobby,
        members: Default::default(),
    })));

    let lobby: Arc<dyn RoomAny> = Arc::clone(&lobby.0) as _;

    ws::listen(addr, |sender| {
        let lobby = Arc::clone(&lobby);

        lobby.add(sender.clone(), Box::new(R::Guest::default()));

        Handler {
            sender,
            room: lobby,
        }
    })
    .unwrap()
}
