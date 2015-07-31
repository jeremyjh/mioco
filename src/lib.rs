// Copyright 2015 Dawid Ciężarkiewicz <dpc@dpc.pw>
// See LICENSE-MPL2 file for more information.

//! Scalable, asynchronous IO coroutine-based handling (aka MIO COroutines).
//!
//! Using `mioco` you can handle scalable, asynchronous [`mio`][mio]-based IO, using set of synchronous-IO
//! handling functions. Based on asynchronous [`mio`][mio] events `mioco` will cooperatively schedule your
//! handlers.
//!
//! You can think of `mioco` as of *Node.js for Rust* or *[green threads][green threads] on top of [`mio`][mio]*.
//!
//! [green threads]: https://en.wikipedia.org/wiki/Green_threads
//! [mio]: https://github.com/carllerche/mio
//!
//! See `examples/echo.rs` for an example TCP echo server.
//!
/*!
```
// MAKE_DOC_REPLACEME
```
*/

#![feature(result_expect)]
#![feature(reflect_marker)]
#![feature(rc_weak)]
#![warn(missing_docs)]

extern crate mio;
extern crate coroutine;
extern crate nix;
#[macro_use]
extern crate log;

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::io;

use mio::{TryRead, TryWrite, Token, Handler, EventLoop, EventSet};
use std::any::Any;
use std::marker::{PhantomData, Reflect};
use mio::util::Slab;


/// Read/Write/Both
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RW {
    /// Read
    Read,
    /// Write
    Write,
    /// Any / Both (depends on context)
    Both,
    /// Something else
    Notify,
}

impl RW {
    fn has_read(&self) -> bool {
        match *self {
            RW::Read | RW::Both => true,
            RW::Write | RW::Notify => false,
        }
    }

    fn has_write(&self) -> bool {
        match *self {
            RW::Write | RW::Both => true,
            RW::Read | RW::Notify => false,
        }
    }
}

/// Last Event
///
/// Read and/or Write + index of the handle in the order of `wrap` calls.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LastEvent {
    index : EventSourceIndex,
    rw : RW,
}

impl LastEvent {
    /// Index of the EventSourceShared handle
    pub fn index(&self) -> EventSourceIndex {
        self.index
    }

    /// Was the event a read
    pub fn has_read(&self) -> bool {
        self.rw.has_read()
    }

    /// Was the event a write
    pub fn has_write(&self) -> bool {
        self.rw.has_write()
    }
}

/// State of `mioco` coroutine
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum State {
    BlockedOn(RW),
    Running,
    Finished,
}

/// `mioco` can work on any type implementing this trait
pub trait Evented : mio::Evented + Any {
    /// Convert to &Any
    fn as_any(&self) -> &Any;
    /// Convert to &mut Any
    fn as_any_mut(&mut self) -> &mut Any;
    /// Implement to receive messages sent from EventSource.
    fn notify(&mut self, handle: MiocoHandle, msg: Message);
}

/// For any non-evented type which needs to be notified through the event loop.

/// Notified types can have co-routines blocked/resume just like an I/O event source;
/// Call `wait_notify` to block the co-routine until a particular instance has been notified.
/// Use `wrap_notified` to get a Sender handle as well as wrapped TypedEventSource.
pub trait Notified {
    /// Implement this to receive notifications for a particular source.
    fn notify(&mut self, handle: MiocoHandle, msg: Message);
}

impl<T> Evented for T
where T : mio::Evented+Reflect+'static {
    fn as_any(&self) -> &Any {
        self as &Any
    }

    fn as_any_mut(&mut self) -> &mut Any {
        self as &mut Any
    }

    fn notify(&mut self, _handle: MiocoHandle, _msg: Message) {}
}

type RefCoroutine = Rc<RefCell<Coroutine>>;

/// `mioco` coroutine
///
/// Referenced by EventSourceShared running within it.
struct Coroutine {
    /// Coroutine of Coroutine itself. Stored here so it's available
    /// through every handle and `Coroutine` itself without referencing
    /// back
    handle : Option<coroutine::coroutine::Handle>,

    /// Current state
    state : State,

    /// Last event that resumed the coroutine
    last_event: LastEvent,

    /// All handles, weak to avoid `Rc`-cycle
    io : Vec<Weak<RefCell<EventSourceShared>>>,

    /// Mask of handle indexes that we're blocked on
    blocked_on_mask : u32,

    /// Mask of handle indexes that are registered in Server
    registered_mask : u32,

    /// `Server` shared data that this `Coroutine` is running in
    server_shared : RefServerShared,

    /// Newly spawned `Coroutine`-es
    children_to_start : Vec<RefCoroutine>,
}


impl Coroutine {
    fn new(server : RefServerShared) -> Coroutine {
        Coroutine {
            state: State::Running,
            handle: None,
            last_event: LastEvent{ rw: RW::Read, index: EventSourceIndex(0)},
            io: Vec::with_capacity(4),
            blocked_on_mask: 0,
            registered_mask: 0,
            server_shared: server,
            children_to_start: Vec::new(),
        }
    }

    /// After `resume()` on the `Coroutine.handle` finished,
    /// the `Coroutine` have blocked or finished and we need to
    /// perform the following maintenance
    fn after_resume<H>(&mut self, event_loop: &mut EventLoop<H>)
        where H : Handler
    {
        // If there were any newly spawned child-coroutines: start them now
        for coroutine in &self.children_to_start {
            let handle = {
                let co = coroutine.borrow_mut();
                co.handle.as_ref().map(|c| c.clone()).unwrap()
            };
            trace!("Resume new child coroutine");
            handle.resume().expect("resume() failed");
            {
                let mut co = coroutine.borrow_mut();
                trace!("Reregister new child coroutine");
                co.reregister(event_loop);
            }
        }
        self.children_to_start.clear();

        trace!("Reregister coroutine");
        self.reregister(event_loop);
    }

    fn reregister<H>(&mut self, event_loop: &mut EventLoop<H>)
        where H : Handler
    {
        if self.state == State::Finished {
            debug!("Coroutine: deregistering");
            self.deregister_all(event_loop);
            let mut shared = self.server_shared.borrow_mut();
            shared.coroutines_no -= 1;
            if shared.coroutines_no == 0 {
                debug!("Shutdown event loop - 0 coroutines left");
                event_loop.shutdown();
            }
        } else {
            self.reregister_blocked_on(event_loop)
        }
    }

    fn deregister_all<H>(&mut self, event_loop: &mut EventLoop<H>)
        where H : Handler
    {
        let mut shared = self.server_shared.borrow_mut();

        for i in 0..self.io.len() {
            let io = self.io[i].upgrade().unwrap();
            let mut io = io.borrow_mut();
            io.deregister(event_loop);
            trace!("Removing source token={:?}", io.token);
            shared.sources.remove(io.token).expect("cleared empty slot");
        }
    }

    fn reregister_blocked_on<H>(&mut self, event_loop: &mut EventLoop<H>)
        where H : Handler
    {

        let rw = match self.state {
            State::BlockedOn(rw) => rw,
            _ => panic!("This should not happen"),
        };

        // TODO: count leading zeros + for i in 0..32 {
        for i in 0..self.io.len() {
            if (self.blocked_on_mask & (1 << i)) != 0 {
                let io = self.io[i].upgrade().unwrap();
                let mut io = io.borrow_mut();
                io.reregister(event_loop, rw);
            } else if (self.registered_mask & (1 << i)) != 0 {
                let io = self.io[i].upgrade().unwrap();
                let io = io.borrow();
                io.unreregister(event_loop);
            }
        }

        self.registered_mask = self.blocked_on_mask;
        self.blocked_on_mask = 0;
    }
}

type RefEventSourceShared = Rc<RefCell<EventSourceShared>>;

enum Source {
    Evented(Box<Evented+'static>),
    Notified(Box<Notified+'static>)
}

/// Wrapped mio IO (mio::Evented+TryRead+TryWrite)
///
/// `Handle` is just a cloneable reference to this struct
struct EventSourceShared {
    coroutine: RefCoroutine,
    token: Token,
    index: usize, /// Index in MiocoHandle::handles
    io : Source,
    peer_hup: bool,
    registered: bool,
}

impl EventSourceShared {
    /// Handle `hup` condition
    fn hup<H>(&mut self, _event_loop: &mut EventLoop<H>, _token: Token)
        where H : Handler {
            self.peer_hup = true;
        }

    /// Reregister oneshot handler for the next event
    fn reregister<H>(&mut self, event_loop: &mut EventLoop<H>, rw : RW)
        where H : Handler {
            let io = match self.io {
                Source::Evented(ref io) => io,
                Source::Notified(_) => return
            };

            let mut interest = mio::EventSet::none();

            if !self.peer_hup {
                interest = interest | mio::EventSet::hup();

                if rw.has_read() {
                    interest = interest | mio::EventSet::readable();
                }
            }

            if rw.has_write() {
                interest = interest | mio::EventSet::writable();
            }

            if !self.registered {
                self.registered = true;
                event_loop.register_opt(
                    &**io, self.token,
                    interest,
                    mio::PollOpt::edge(),
                    ).expect("register_opt failed");
            } else {
                 event_loop.reregister(
                     &**io, self.token,
                     interest, mio::PollOpt::edge() | mio::PollOpt::oneshot()
                     ).ok().expect("reregister failed")
             }
        }

    /// Un-reregister events we're not interested in anymore
    fn unreregister<H>(&self, event_loop: &mut EventLoop<H>)
        where H : Handler {
            let io = match self.io {
                Source::Evented(ref io) => io,
                Source::Notified(_) => return
            };
            let interest = mio::EventSet::none();

            debug_assert!(self.registered);

            event_loop.reregister(
                &**io, self.token,
                interest, mio::PollOpt::edge() | mio::PollOpt::oneshot()
                ).ok().expect("reregister failed")
        }

    /// Un-reregister events we're not interested in anymore
    fn deregister<H>(&mut self, event_loop: &mut EventLoop<H>)
        where H : Handler {
            let io = match self.io {
                Source::Evented(ref io) => io,
                Source::Notified(_) => return
            };
            if self.registered {
                event_loop.deregister(&**io).expect("deregister failed");
                self.registered = false;
            }
        }
}

/// `mioco` wrapper over raw structure implementing `mio::Evented` trait
#[derive(Clone)]
struct EventSource {
    inn : RefEventSourceShared,
}

/// `mioco` wrapper over raw mio IO structure
///
/// Create using `MiocoHandle::wrap()`
///
/// It implements standard library `Read` and `Write` and other
/// blocking-semantic operations, that switch and resume handler function
/// to build cooperative scheduling on top of asynchronous operations.
#[derive(Clone)]
pub struct TypedEventSource<T> {
    inn : RefEventSourceShared,
    _t: PhantomData<T>,
}

/// Index identification of a `TypedEventSource` used in `select`-like operations.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EventSourceIndex(usize);

impl EventSourceIndex {
    fn as_usize(&self) -> usize {
        self.0
    }
}

impl<T> TypedEventSource<T>
where T : Reflect+'static {
    /// Mark the `EventSource` blocked and block until `Server` does
    /// not wake us up again.
    fn block_on(&self, rw : RW) {
        {
            let inn = self.inn.borrow();
            inn.coroutine.borrow_mut().state = State::BlockedOn(rw);
            inn.coroutine.borrow_mut().blocked_on_mask = 1 << inn.index;
        }
        trace!("coroutine blocked on {:?}", rw);
        coroutine::Coroutine::block();
        //TODO: we can block now without having a previous I/O Event (wait_notify) - but
        // why did we have this guard to begin with, and do we need to preserve it somehow?
        // {
        //     let inn = self.inn.borrow_mut();
        //     debug_assert!(rw.has_read() || inn.coroutine.borrow().last_event.has_write());
        //     debug_assert!(rw.has_write() || inn.coroutine.borrow().last_event.has_read());
        //     debug_assert!(inn.coroutine.borrow().last_event.index().as_usize() == inn.index);
        // }
    }
}

impl<T> TypedEventSource<T>
where T : Evented+Reflect+'static {
    /// Access raw mio type
    pub fn with_raw<F>(&self, f : F)
        where F : Fn(&T) {
        match self.inn.borrow().io {
            Source::Evented(ref io) =>
                f(io.as_any().downcast_ref::<T>().unwrap()),
            Source::Notified(_) =>
                panic!("Notified source implements Evented!?")
        }
    }

    /// Access mutable raw mio type
    pub fn with_raw_mut<F>(&mut self, f : F)
        where F : Fn(&mut T) {
        match self.inn.borrow_mut().io {
            Source::Evented(ref mut io) =>
                f(io.as_any_mut().downcast_mut::<T>().unwrap()),
            Source::Notified(_) =>
                panic!("Notified Source implements Evented!?")
        }
    }

    /// Index identificator of a `TypedEventSource`
    pub fn index(&self) -> EventSourceIndex {
        EventSourceIndex(self.inn.borrow().index)
    }
}

impl<T> TypedEventSource<T>
where T : Notified+Reflect+'static {
    ///Block the current co-routine until a notification is received for this
    ///source.
    pub fn wait_notify(&self){
        self.block_on(RW::Notify)
    }

    ///Get a clonable, thread-safe sender object based on `mio::Sender`.
    pub fn channel(&self) -> Sender {
        let inn = self.inn.borrow();
        let co = inn.coroutine.borrow();
        let server = co.server_shared.borrow();
        Sender::new(inn.token,(server.channel.clone()))
    }

    /// Send a message which will be routed to this particular `TypedEventSource<T>` instance's
    /// `notify` method.
    pub fn send(&self, msg: Message) -> Result<(), mio::NotifyError<(Token, Message)>> {
        let inn = self.inn.borrow();
        let co = inn.coroutine.borrow();
        let server = co.server_shared.borrow();
        server.channel.send((inn.token,msg))
    }
}

impl EventSource {
    /// Readable event handler
    ///
    /// This corresponds to `mio::Handler::readable()`.
    pub fn ready<H>(&mut self, event_loop: &mut EventLoop<H>, token: Token, events : EventSet)
    where H : Handler {
        if events.is_hup() {
            let mut inn = self.inn.borrow_mut();
            inn.hup(event_loop, token);
        }

        // Wake coroutine on HUP, as it was read, to potentially let it fail the read and move on
        let event = match (events.is_readable() | events.is_hup(), events.is_writable()) {
            (true, true) => RW::Both,
            (true, false) => RW::Read,
            (false, true) => RW::Write,
            (false, false) => panic!(),
        };

        self.resume(event_loop, event);
    }

    pub fn notify<H>(&mut self, event_loop: &mut EventLoop<H>, msg: Message)
    where H : Handler {
        {
            let mut inn = self.inn.borrow_mut();
            let handle = MiocoHandle {coroutine: inn.coroutine.clone()};
            match inn.io {
                Source::Notified(ref mut io) => io.notify(handle, msg),
                Source::Evented(_) => return
            }
        }
        self.resume(event_loop, RW::Notify);
    }

    fn resume<H>(&mut self, event_loop: &mut EventLoop<H>, event: RW)
    where H : Handler {
        let my_index = {
            let inn = self.inn.borrow();
            let index = inn.index;
            let mut co = inn.coroutine.borrow_mut();
            co.blocked_on_mask &= !(1 << index);
            index
        };

        let handle = {
            let inn = self.inn.borrow();
            let coroutine_handle = inn.coroutine.borrow().handle.as_ref().map(|c| c.clone()).unwrap();
            inn.coroutine.borrow_mut().state = State::Running;
            if event != RW::Notify {
                inn.coroutine.borrow_mut().last_event = LastEvent {
                    rw: event,
                    index: EventSourceIndex(my_index),
                };
            }
            coroutine_handle
        };

        handle.resume().expect("resume() failed");

        let coroutine = {
            let inn = &self.inn.borrow();
            inn.coroutine.clone()
        };

        let mut co = coroutine.borrow_mut();

        co.after_resume(event_loop);
    }

}

impl<T> TypedEventSource<T>
where T : mio::TryAccept+Evented+Reflect+'static {
    /// Block on accept
    pub fn accept(&self) -> io::Result<T::Output> {
        loop {
            let res = {
                match self.inn.borrow_mut().io {
                    Source::Evented(ref mut io) =>
                        io.as_any_mut().downcast_mut::<T>().unwrap().accept(),
                    Source::Notified(_) =>
                        panic!("Notified source implements Evented!?")
                }
            };

            match res {
                Ok(None) => {
                    self.block_on(RW::Read)
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }
}

impl<T> std::io::Read for TypedEventSource<T>
where T : TryRead+Evented+Reflect+'static {
    /// Block on read
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let res = {
                match self.inn.borrow_mut().io {
                    Source::Evented(ref mut io) =>
                        io.as_any_mut().downcast_mut::<T>().unwrap().try_read(buf),
                    Source::Notified(_) =>
                        panic!("Notified source implements Evented!?")
                }
            };

            match res {
                Ok(None) => {
                    self.block_on(RW::Read)
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }
}

impl<T> std::io::Write for TypedEventSource<T>
where T : TryWrite+Reflect+'static {
    /// Block on write
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            let res = {
                match self.inn.borrow_mut().io {
                    Source::Evented(ref mut io) =>
                        io.as_any_mut().downcast_mut::<T>().unwrap().try_write(buf),
                    Source::Notified(_) =>
                        panic!("Notified source implements Evented!?")
                }
            };

            match res {
                Ok(None) => {
                    self.block_on(RW::Write)
                },
                Ok(Some(r)) => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }

    /// Flush. This currently does nothing
    ///
    /// TODO: Should we do something with the flush? --dpc */
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Mioco Handle
///
/// Use this from withing coroutines to perform `mioco`-provided functionality
pub struct MiocoHandle {
    coroutine : Rc<RefCell<Coroutine>>,
}

fn select_impl_set_mask_from_indices(indices : &[EventSourceIndex], blocked_on_mask : &mut u32) {
    {
        *blocked_on_mask = 0;
        for &index in indices {
            *blocked_on_mask |= 1u32 << index.as_usize();
        }
    }
}

fn select_impl_set_mask_rc_handles(handles : &[Weak<RefCell<EventSourceShared>>], blocked_on_mask : &mut u32) {
    {
        *blocked_on_mask = 0;
        for handle in handles {
            let io = handle.upgrade().unwrap();
            *blocked_on_mask |= 1u32 << io.borrow().index;
        }
    }
}

impl MiocoHandle {

    /// Create a `mioco` coroutine handler
    ///
    /// `f` is routine handling connection. It must not use any real blocking-IO operations, only
    /// `mioco` provided types (`TypedEventSource`) and `MiocoHandle` functions. Otherwise `mioco`
    /// cooperative scheduling can block on real blocking-IO which defeats using mioco.
    pub fn spawn<F>(&self, f : F)
        where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static {
            let coroutine_ref = spawn_impl(f, self.coroutine.borrow().server_shared.clone());
            self.coroutine.borrow_mut().children_to_start.push(coroutine_ref);
        }

    /// Register `mio`'s native io type to be used within `mioco` coroutine
    ///
    /// Consumes the `io`, returns a mioco wrapper over it. Use this wrapped IO
    /// to perform IO.
    pub fn wrap<T : 'static>(&mut self, io : T) -> TypedEventSource<T>
    where T : Evented {
        self.add_source::<T>(Source::Evented(Box::new(io)))
    }

    ///TODO: could we dispatch this dynamically rather than the separate method for notify?
    pub fn wrap_notified<T : 'static>(&mut self, io : T) -> TypedEventSource<T>
    where T : Notified {
        self.add_source::<T>(Source::Notified(Box::new(io)))
    }

    fn add_source<T : 'static>(&mut self, source: Source) -> TypedEventSource<T>{
        let token = {
            let co = self.coroutine.borrow();
            let mut shared = co.server_shared.borrow_mut();
            shared.sources.insert_with(|token| {
                EventSource {
                    inn: Rc::new(RefCell::new(
                                    EventSourceShared {
                                        coroutine: self.coroutine.clone(),
                                        io: source,
                                        token: token,
                                        peer_hup: false,
                                        index: self.coroutine.borrow().io.len(),
                                        registered: false,
                                    }
                                    )),
                }
            })
        }.expect("run out of tokens");
        trace!("Added source token={:?}", token);
        let io = {
            let co = self.coroutine.borrow();
            let shared = co.server_shared.borrow_mut();
            shared.sources[token].inn.clone()
        };
        let handle = TypedEventSource {
            inn: io.clone(),
            _t: PhantomData,
        };

        self.coroutine.borrow_mut().io.push(io.clone().downgrade());

        handle
    }

    /// Wait till a read event is ready
    fn select_impl(&mut self, rw : RW) -> LastEvent {
        self.coroutine.borrow_mut().state = State::BlockedOn(rw);
        coroutine::Coroutine::block();
        debug_assert!(self.coroutine.borrow().state == State::Running);

        self.coroutine.borrow().last_event
    }

    /// Wait till an event is ready
    ///
    /// The returned value contains event type and the index id of the `TypedEventSource`.
    /// See `TypedEventSource::index()`.
    pub fn select(&mut self) -> LastEvent {
        {
            let Coroutine {
                ref io,
                ref mut blocked_on_mask,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_rc_handles(&**io, blocked_on_mask);
        }
        self.select_impl(RW::Both)
    }

    /// Wait till a read event is ready
    ///
    /// See `MiocoHandle::select`.
    pub fn select_read(&mut self) -> LastEvent {
        {
            let Coroutine {
                ref io,
                ref mut blocked_on_mask,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_rc_handles(&**io, blocked_on_mask);
        }
        self.select_impl(RW::Read)
    }

    /// Wait till a read event is ready.
    ///
    /// See `MiocoHandle::select`.
    pub fn select_write(&mut self) -> LastEvent {
        {
            let Coroutine {
                ref io,
                ref mut blocked_on_mask,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_rc_handles(&**io, blocked_on_mask);
        }
        self.select_impl(RW::Write)
    }

    /// Wait till any event is ready on a set of Handles.
    ///
    /// See `TypedEventSource::index()`.
    /// See `MiocoHandle::select()`.
    pub fn select_from(&mut self, indices : &[EventSourceIndex]) -> LastEvent {
        {
            let Coroutine {
                ref mut blocked_on_mask,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_from_indices(indices, blocked_on_mask);
        }

        self.select_impl(RW::Both)
    }

    /// Wait till write event is ready on a set of Handles.
    ///
    /// See `MiocoHandle::select_from`.
    pub fn select_write_from(&mut self, indices : &[EventSourceIndex]) -> LastEvent {
        {
            let Coroutine {
                ref mut blocked_on_mask,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_from_indices(indices, blocked_on_mask);
        }

        self.select_impl(RW::Write)
    }

    /// Wait till read event is ready on a set of Handles.
    ///
    /// See `MiocoHandle::select_from`.
    pub fn select_read_from(&mut self, indices : &[EventSourceIndex]) -> LastEvent {
        {
            let Coroutine {
                ref mut blocked_on_mask,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_from_indices(indices, blocked_on_mask);
        }

        self.select_impl(RW::Read)
    }
}

type RefServerShared = Rc<RefCell<ServerShared>>;

/// Data belonging to `Server`, but referenced and manipulated by `Coroutine`-es
/// belonging to it.
struct ServerShared {
    /// Slab allocator
    /// TODO: dynamically growing slab would be better; or a fast hashmap?
    /// FIXME: See https://github.com/carllerche/mio/issues/219 . Using an allocator
    /// in which just-deleted entries are not potentially reused right away might prevent
    /// potentical sporious wakeups on newly allocated entries.
    sources : Slab<EventSource>,

    /// Number of `Coroutine`-s running in the `Server`.
    coroutines_no : u32,

    /// The mio loop
    channel: MioSender
}

impl ServerShared {
    fn new(channel: MioSender) -> ServerShared {
        ServerShared {
            sources: Slab::new(1024),
            coroutines_no: 0,
            channel: channel,
        }
    }
}

fn spawn_impl<F>(f : F, server : RefServerShared) -> RefCoroutine
where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static {


    struct SendFnOnce<F>
    {
        f : F
    }

    // We fake the `Send` because `mioco` guarantees serialized
    // execution between coroutines, switching between them
    // only in predefined points.
    unsafe impl<F> Send for SendFnOnce<F>
        where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static
        {

        }

    struct SendRefCoroutine {
        coroutine: RefCoroutine,
    }

    // Same logic as in `SendFnOnce` applies here.
    unsafe impl Send for SendRefCoroutine { }

    trace!("Coroutine: spawning");
    server.borrow_mut().coroutines_no += 1;

    let coroutine_ref = Rc::new(RefCell::new(Coroutine::new(server)));

    let sendref = SendRefCoroutine {
        coroutine: coroutine_ref.clone(),
    };

    let send_f = SendFnOnce {
        f: f,
    };

    let coroutine_handle = coroutine::coroutine::Coroutine::spawn(move || {
        trace!("Coroutine: started");
        let mut mioco_handle = MiocoHandle {
            coroutine: sendref.coroutine,
        };

        let SendFnOnce { f } = send_f;

        let _res = f(&mut mioco_handle);

        mioco_handle.coroutine.borrow_mut().state = State::Finished;
        mioco_handle.coroutine.borrow_mut().blocked_on_mask = 0;
        trace!("Coroutine: finished");
    });

    coroutine_ref.borrow_mut().handle = Some(coroutine_handle);

    coroutine_ref
}

/// `Server` registered in `mio::EventLoop` and implementing `mio::Handler`.
struct Server {
    shared : RefServerShared,
}

impl Server {
    fn new(shared : RefServerShared) -> Server {
        Server {
            shared: shared,
        }
    }
}

/// Wrapper for msg delivered through `mioco::Evented::notify`.
pub type Message = Box<Any+'static+Send>;

/// Sends notify messages to the mioco Event Loop.
pub type MioSender = mio::Sender<<Server as mio::Handler>::Message>;


#[derive(Clone)]
/// Wrapper around `mio::Sender` which will route messages to the correct
/// event source instance.
pub struct Sender {
    inner: MioSender,
    token: Token
}
// mio::Sender is Send
unsafe impl Send for Sender { }

impl Sender{
    fn new(token: Token, sender: MioSender) -> Sender{
       Sender {token: token, inner: sender}
    }
    /// Send a message which will be routed to the `TypedEventSource<T>` instance which created this
    /// instance of `Sender`.
    pub fn send(&self, msg: Message) -> Result<(), mio::NotifyError<(Token, Message)>> {
        self.inner.send((self.token,msg))
    }
}


impl mio::Handler for Server {
    type Timeout = usize;
    type Message = (Token, Message);

    fn ready(&mut self, event_loop: &mut mio::EventLoop<Server>, token: mio::Token, events: mio::EventSet) {
        // It's possible we got an event for a Source that was deregistered
        // by finished coroutine. In case the token is already occupied by
        // different source, we will wake it up needlessly. If it's empty, we just
        // ignore the event.
        trace!("Server::ready(token={:?})", token);
        let mut source = match self.shared.borrow().sources.get(token) {
            Some(source) => source.clone(),
            None => {
                trace!("Server::ready() ignored");
                return
            },
        };
        source.ready(event_loop, token, events);
        trace!("Server::ready finished");
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Self>, msg: Self::Message) {
        let (token, msg) = msg;
        trace!("Server::notify(token={:?})", token);
        let mut source = match self.shared.borrow().sources.get(token) {
            Some(source) => source.clone(),
            None => {
                trace!("Server::notify() ignored");
                return
            },
        };
        source.notify(event_loop, msg);
        trace!("Server::notify finished");
    }
}

/// Start mioco handling
///
/// Takes a starting handler function that will be executed in `mioco` environment.
///
/// Will block until `mioco` is finished - there are no more handlers to run.
///
/// See `MiocoHandle::spawn()`.
pub fn start<F>(f : F)
    where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static,
{
    let mut event_loop = EventLoop::new().expect("new EventLoop");

    let shared = Rc::new(RefCell::new(ServerShared::new(event_loop.channel())));
    let mut server = Server::new(shared.clone());
    let coroutine_ref = spawn_impl(f, shared);

    let coroutine_handle = coroutine_ref.borrow().handle.as_ref().map(|c| c.clone()).unwrap();

    trace!("Initial resume");
    coroutine_handle.resume().expect("resume() failed");
    {
        let mut co = coroutine_ref.borrow_mut();
        co.after_resume(&mut event_loop);
    }

    trace!("Start event loop");
    event_loop.run(&mut server).unwrap();
}
