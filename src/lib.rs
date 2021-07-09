//! # `ws-hotel`
//!
//! _Your websocket server, with rooms._

use std::any::Any;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use ws::util::Token;
use ws::Sender;

pub use ws::{self, CloseCode, Handshake, Message, Result};

/// A room in which websocket clients can be moved
///
/// It effectively contains a user-provided [`RoomHandler`] as R as well as a set of users that
/// are in the room.
pub struct Room<R: RoomHandler>(Arc<Mutex<RoomInner<R>>>);

struct RoomInner<R: RoomHandler> {
    handler: R,
    members: Vec<(R::Guest, Sender)>,
}

impl<R: RoomHandler> RoomInner<R> {
    fn with_context<F: FnOnce(&mut R, Context<R::Guest>) -> O, O>(
        &mut self,
        sender: &Sender,
        f: F,
    ) -> O {
        // TODO: remove
        //     Instead of allocating, use unsafe wrapper around HashMap that allows value mutation
        //     but no other kind of mutation. Thus, it will be possible to use `broadcast` or
        //     access the list of Senders in the room without preventing mutable access to the
        //     current identity.
        let todo = self
            .members
            .iter()
            .map(|(_, v)| (PhantomData, v.clone()))
            .collect::<Vec<_>>();

        let cx = Context {
            sender,
            members: &todo,
            members_a: &mut self.members,
            me: (sender.token(), sender.connection_id()),
        };

        f(&mut self.handler, cx)
    }
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

impl<R: RoomHandler> Debug for Room<R>
where
    R: Debug,
    R::Guest: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let lock = self.0.lock().unwrap();

        f.debug_struct("Room")
            .field("[handler]", &lock.handler)
            .field("[members]", &lock.members)
            .finish()
    }
}

/// Two rooms are equal if their inner [Arc] points to the same underlying data.
impl<R: RoomHandler> PartialEq for Room<R> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<R: RoomHandler> Eq for Room<R> {}

pub struct Relocation(Arc<dyn RoomAny>, Box<dyn Any>);

impl Relocation {
    #[must_use]
    pub fn new<R: RoomHandler>(room: &Room<R>, identity: R::Guest) -> Self
    where
        R: 'static,
        R::Guest: 'static,
    {
        Self(Arc::clone(&room.0) as _, Box::new(identity) as _)
    }
}

pub type ResultRelocation = ws::Result<Option<Relocation>>;

trait RoomAny {
    fn on_join(&self, sender: &Sender) -> ResultRelocation;
    fn on_message(&self, sender: &Sender, msg: Message) -> ResultRelocation;
    fn on_leave(&self, sender: &Sender, code_and_reason: Option<(CloseCode, &str)>);

    fn broadcast(&self, msg: Message) -> ws::Result<()>;

    fn add(&self, sender: Sender, identity: Box<dyn Any>);
    fn remove(&self, sender: &Sender);
}

impl<R: RoomHandler + 'static> RoomAny for Mutex<RoomInner<R>> {
    fn on_join(&self, sender: &Sender) -> ResultRelocation {
        self.lock()
            .unwrap()
            .with_context(sender, move |h, cx| h.on_join(cx))
    }

    fn on_message(&self, sender: &Sender, msg: Message) -> ResultRelocation {
        self.lock()
            .unwrap()
            .with_context(sender, move |h, cx| h.on_message(cx, msg))
    }

    fn on_leave(&self, sender: &Sender, code_and_reason: Option<(CloseCode, &str)>) {
        self.lock()
            .unwrap()
            .with_context(sender, move |h, cx| h.on_leave(cx, code_and_reason))
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
        self.lock().unwrap().members.push((identity, sender));
    }

    fn remove(&self, sender: &Sender) {
        let mut lock = self.lock().unwrap();

        let index = lock
            .members
            .iter()
            .position(|(_, s)| s == sender)
            .expect("attempted to remove member, but it wasn't here");

        lock.members.swap_remove(index);
    }
}

pub struct Context<'a, 'm, Guest> {
    sender: &'a Sender,
    members: &'a [(PhantomData<Guest>, Sender)],
    members_a: &'m mut [(Guest, Sender)],
    me: (Token, u32),
}

impl<Guest> Context<'_, '_, Guest> {
    /// Returns the identity of the client associated with this [Context]
    pub fn identity(&mut self) -> &mut Guest {
        // TODO memoize this function ? probably requires unsafe code

        let sender = self.me;

        &mut self
            .members_a
            .iter_mut()
            .find(move |(_, s)| (s.token(), s.connection_id()) == sender)
            .expect("guest not in room")
            .0
    }

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
}

impl<Guest: Debug> Debug for Context<'_, '_, Guest> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // TODO memoize ? move into a non-mut function ? idk

        let sender = self.me;

        let identity = &self
            .members_a
            .iter()
            .find(move |(_, s)| (s.token(), s.connection_id()) == sender)
            .expect("guest not in room")
            .0;

        f.debug_struct("Context")
            .field("sender", &self.sender)
            .field("[identity]", identity)
            .finish()
    }
}

struct Handler {
    sender: Sender,
    room: Arc<dyn RoomAny>,
}

impl Handler {
    pub fn relocate(&mut self, mut r: Option<Relocation>) -> ws::Result<()> {
        let sender = &self.sender;

        while let Some(Relocation(room, identity)) = r.take() {
            self.room.on_leave(sender, None);
            self.room.remove(sender);
            self.room = room;

            self.room.add(sender.clone(), identity);
            r = self.room.on_join(sender)?;
        }

        Ok(())
    }
}

impl ws::Handler for Handler {
    fn on_open(&mut self, _shake: Handshake) -> ws::Result<()> {
        // TODO let user build their `Guest` from the handshake

        Ok(())
    }

    fn on_message(&mut self, msg: Message) -> ws::Result<()> {
        self.room
            .on_message(&self.sender, msg)
            .and_then(|r| self.relocate(r))
    }

    fn on_close(&mut self, code: CloseCode, reason: &str) {
        self.room.on_leave(&self.sender, Some((code, reason)));
        self.room.remove(&self.sender);
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

    fn on_join(&mut self, _cx: Context<Self::Guest>) -> ResultRelocation {
        Ok(None)
    }

    fn on_message(&mut self, cx: Context<Self::Guest>, msg: Message) -> ResultRelocation;

    fn on_leave(&mut self, _cx: Context<Self::Guest>, _code_and_reason: Option<(CloseCode, &str)>) {
    }
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

impl<F, G> Debug for AdHoc<F, G> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple(&format!("AdHoc<_, {}>", std::any::type_name::<G>()))
            .finish()
    }
}

impl<Guest, F: FnMut(Context<Guest>, Message) -> ResultRelocation> RoomHandler for AdHoc<F, Guest> {
    type Guest = Guest;

    fn on_message(&mut self, cx: Context<Self::Guest>, msg: Message) -> ResultRelocation {
        self.0(cx, msg)
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
