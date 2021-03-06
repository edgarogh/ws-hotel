//! # `ws-hotel`
//!
//! _Your websocket server, with rooms._

#![feature(arc_new_cyclic)]

use std::any::Any;
use std::fmt::{Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::net::ToSocketAddrs;
use std::ops::Deref;
use std::sync::{Arc, Mutex, Weak};
use ws::util::Token;
use ws::Sender;

pub use ws::{self, CloseCode, Handshake, Message, Result};

/// A room in which websocket clients can be moved
///
/// It effectively contains a user-provided [`RoomHandler`] as R as well as a set of users that
/// are in the room.
pub struct RoomRef<R: RoomHandler>(Arc<Mutex<Room<R>>>);

impl<R: RoomHandler> RoomRef<R> {
    pub fn downgrade(&self) -> RoomRefWeak<R> {
        RoomRefWeak(Arc::downgrade(&self.0))
    }

    /// Run code that needs access to the internal `RoomHandler`.
    ///
    /// Accessing it requires locking a Mutex, beware of deadlocks !
    #[inline]
    pub fn with<F: FnOnce(&mut R) -> T, T>(&self, f: F) -> T {
        f(&mut self.0.lock().unwrap().handler)
    }
}

impl<R: RoomHandler> Clone for RoomRef<R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<R: RoomHandler> From<R> for RoomRef<R> {
    fn from(handler: R) -> Self {
        Room::new(handler)
    }
}

/// Two rooms are equal if their inner [Arc] points to the same underlying data.
impl<R: RoomHandler> PartialEq for RoomRef<R> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<R: RoomHandler> Eq for RoomRef<R> {}

impl<R: RoomHandler> Hash for RoomRef<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let ptr = Arc::deref(&self.0) as *const _ as usize;
        ptr.hash(state)
    }
}

impl<R: RoomHandler> Debug for RoomRef<R>
where
    R: Debug,
    R::Guest: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RoomRef").field(&self.0).finish()
    }
}

/// A weak [RoomRef] equivalent. Like with an [Arc], it must be [upgrade][RoomRefWeak::upgrade]d to
/// be usable.
pub struct RoomRefWeak<R: RoomHandler>(Weak<Mutex<Room<R>>>);

impl<R: RoomHandler> RoomRefWeak<R> {
    pub fn upgrade(&self) -> Option<RoomRef<R>> {
        self.0.upgrade().map(RoomRef)
    }
}

impl<R: RoomHandler> Clone for RoomRefWeak<R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<R: RoomHandler> PartialEq for RoomRefWeak<R> {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }
}

impl<R: RoomHandler> PartialEq<RoomRef<R>> for RoomRefWeak<R> {
    fn eq(&self, other: &RoomRef<R>) -> bool {
        Weak::ptr_eq(&self.0, &Arc::downgrade(&other.0))
    }
}

impl<R: RoomHandler> Eq for RoomRefWeak<R> {}

impl<R: RoomHandler> Hash for RoomRefWeak<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let ptr = self.0.as_ptr() as usize;
        ptr.hash(state)
    }
}

impl<R: RoomHandler> Debug for RoomRefWeak<R>
where
    R: Debug,
    R::Guest: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RoomRefWeak").field(&self.0).finish()
    }
}

pub struct Room<R: RoomHandler> {
    self_ref: RoomRefWeak<R>,

    handler: R,
    members: Vec<(R::Guest, Sender)>,
}

impl<R: RoomHandler> Room<R> {
    fn with_context<F: FnOnce(&mut R, Context<R>) -> O, O>(&mut self, sender: &Sender, f: F) -> O {
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
            room: &self.self_ref,
            sender,
            members: &todo,
            members_a: &mut self.members,
            me: (sender.token(), sender.connection_id()),
        };

        f(&mut self.handler, cx)
    }
}

impl<R: RoomHandler> Room<R> {
    /// Constructs a new empty [RoomRef]
    ///
    /// Clients can be moved inside using [`Context::relocate`]:
    ///
    /// ```no_run
    /// // TODO this whole example is broken
    ///
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
    pub fn new(handler: R) -> RoomRef<R> {
        RoomRef(Arc::new_cyclic(|weak| {
            Mutex::new(Room {
                self_ref: RoomRefWeak(weak.clone()),
                handler,
                members: Vec::new(),
            })
        }))
    }
}

impl<R: RoomHandler> Debug for Room<R>
where
    R: Debug,
    R::Guest: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Room")
            .field("handler", &self.handler)
            .field("members", &self.members)
            .finish()
    }
}

pub struct Relocation(Arc<dyn RoomAny>, Box<dyn Any>);

impl Relocation {
    #[must_use]
    pub fn new<R: RoomHandler>(room: &RoomRef<R>, identity: R::Guest) -> Self
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

impl<R: RoomHandler + 'static> RoomAny for Mutex<Room<R>> {
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

pub struct Context<'a, 'm, R: RoomHandler> {
    room: &'a RoomRefWeak<R>,

    sender: &'a Sender,
    members: &'a [(PhantomData<R::Guest>, Sender)],
    members_a: &'m mut [(R::Guest, Sender)],
    me: (Token, u32),
}

impl<R: RoomHandler> Context<'_, '_, R> {
    /// Weak reference to the current room
    pub fn room(&self) -> &RoomRefWeak<R> {
        self.room
    }

    /// Returns the identity of the client associated with this [Context]
    pub fn identity(&mut self) -> &mut R::Guest {
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

    /// Sends a message to everyone in the same room by calling a closure for each member
    pub fn broadcast_with<F: FnMut(&R::Guest) -> M, M: Into<Message>>(
        &self,
        mut f: F,
    ) -> ws::Result<()> {
        self.members_a
            .iter()
            .map(|(identity, sender)| sender.send(f(identity)))
            .collect()
    }
}

impl<R: RoomHandler> Debug for Context<'_, '_, R>
where
    R::Guest: Debug,
{
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
            .finish_non_exhaustive()
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
pub trait RoomHandler: Sized {
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

    fn on_join(&mut self, _cx: Context<Self>) -> ResultRelocation {
        Ok(None)
    }

    fn on_message(&mut self, cx: Context<Self>, msg: Message) -> ResultRelocation;

    fn on_leave(&mut self, _cx: Context<Self>, _code_and_reason: Option<(CloseCode, &str)>) {}
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

impl<Guest, F: FnMut(Context<Self>, Message) -> ResultRelocation> RoomHandler for AdHoc<F, Guest> {
    type Guest = Guest;

    fn on_message(&mut self, cx: Context<Self>, msg: Message) -> ResultRelocation {
        self.0(cx, msg)
    }
}

/// Starts a WebSocket hotel given an address and a default room.
/// This function blocks indefinitely.
///
/// The default room is where clients will be put when connecting the server. Its associated
/// [`RoomHandler::Guest`] type must implement [`Default`], so it can be built implicitly.
pub fn listen<A, I, R>(addr: A, lobby: I)
where
    A: ToSocketAddrs + std::fmt::Debug,
    I: Into<RoomRef<R>>,
    R: RoomHandler + 'static,
    R::Guest: Default + 'static,
{
    let lobby = lobby.into();

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
