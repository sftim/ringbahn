use std::io;
use std::future::Future;
use std::mem;
use std::net::{ToSocketAddrs, SocketAddr};
use std::os::unix::io::{RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::{ready, Stream};
use nix::sys::socket::{SockProtocol, SockFlag};

use crate::drive::demo::DemoDriver;
use crate::Cancellation;
use crate::{Drive, Ring};

use super::TcpStream;

pub struct TcpListener<D: Drive = DemoDriver<'static>> {
    ring: Ring<D>,
    fd: RawFd,
    active: Op,
    addr: Option<Box<iou::SockAddrStorage>>,
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
enum Op {
    Nothing = 0,
    Accept,
    Close,
}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        TcpListener::bind_on_driver(addr, DemoDriver::default())
    }
}

impl<D: Drive> TcpListener<D> {
    pub fn bind_on_driver<A: ToSocketAddrs>(addr: A, driver: D) -> io::Result<TcpListener<D>> {
        let (fd, addr) = super::socket(addr, SockProtocol::Tcp)?;
        let val = &1 as *const libc::c_int as *const libc::c_void;
        let len = mem::size_of::<libc::c_int>() as u32;
        unsafe {
            if libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, val, len) < 0 {
                return Err(io::Error::last_os_error());
            }

            let addr = iou::SockAddr::Inet(nix::sys::socket::InetAddr::from_std(&addr));
            let (addr, addrlen) = addr.as_ffi_pair();
            if libc::bind(fd, addr, addrlen) < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::listen(fd, 128) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let ring = Ring::new(driver);
        Ok(TcpListener {
            active: Op::Nothing,
            addr: None,
            fd, ring,
        })
    }

    pub fn close(&mut self) -> Close<D> where D: Unpin {
        Pin::new(self).close_pinned()
    }

    pub fn close_pinned(self: Pin<&mut Self>) -> Close<D> {
        Close { socket: self }
    }

    fn guard_op(self: Pin<&mut Self>, op: Op) {
        let this = unsafe { Pin::get_unchecked_mut(self) };
        if this.active != Op::Nothing && this.active != op {
            this.cancel();
        }
        this.active = op;
    }

    fn cancel(&mut self) {
        let cancellation = match self.active {
            Op::Accept => {
                unsafe fn callback(addr: *mut (), _: usize) {
                    drop(Box::from_raw(addr as *mut iou::SockAddrStorage))
                }
                unsafe {
                    let addr: &mut iou::SockAddrStorage = &mut **self.addr.as_mut().unwrap();
                    Cancellation::new(addr as *mut iou::SockAddrStorage as *mut (), 0, callback)
                }
            }
            Op::Close   => Cancellation::null(),
            Op::Nothing => return,
        };
        self.active = Op::Nothing;
        self.ring.cancel(cancellation);
    }

    unsafe fn drop_addr(self: Pin<&mut Self>) {
        Pin::get_unchecked_mut(self).addr.take();
    }

    fn ring(self: Pin<&mut Self>) -> Pin<&mut Ring<D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.ring) }
    }

    fn split(self: Pin<&mut Self>) -> (Pin<&mut Ring<D>>, &mut iou::SockAddrStorage) {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            if this.addr.is_none() {
                this.addr = Some(Box::new(iou::SockAddrStorage::uninit()));
            }
            (Pin::new_unchecked(&mut this.ring), &mut **this.addr.as_mut().unwrap())
        }
    }
}

impl<D: Drive + Clone> TcpListener<D> {
    pub fn accept(&mut self) -> Accept<'_, D> where D: Unpin {
        Pin::new(self).accept_pinned()
    }

    pub fn accept_pinned(self: Pin<&mut Self>) -> Accept<'_, D> {
        Accept { socket: self }
    }

    pub fn incoming(&mut self) -> Incoming<'_, D> where D: Unpin {
        Pin::new(self).incoming_pinned()
    }

    pub fn incoming_pinned(self: Pin<&mut Self>) -> Incoming<'_, D> {
        Incoming { accept: self.accept_pinned() }
    }

    pub fn poll_accept(mut self: Pin<&mut Self>, ctx: &mut Context<'_>)
        -> Poll<io::Result<(TcpStream<D>, SocketAddr)>>
    {
        self.as_mut().guard_op(Op::Accept);
        let fd = self.fd;
        let (ring, addr) = self.as_mut().split();
        let fd = ready!(ring.poll(ctx, true, 1, |sqs| unsafe {
            let mut sqe = sqs.single().unwrap();
            sqe.prep_accept(fd, Some(addr), SockFlag::empty());
            sqe
        }))? as RawFd;
        let addr = unsafe {
            let result = addr.as_socket_addr();
            self.as_mut().drop_addr();
            match result? {
                iou::SockAddr::Inet(addr) => addr.to_std(),
                addr => panic!("TcpListener addr cannot be {:?}", addr.family()),
            }
        };

        Poll::Ready(Ok((TcpStream::from_fd(fd, self.ring().clone()), addr)))
    }

}

impl<D: Drive> Drop for TcpListener<D> {
    fn drop(&mut self) {
        match self.active {
            Op::Nothing => unsafe { libc::close(self.fd); }
            _           => self.cancel(),
        }
    }
}

pub struct Accept<'a, D: Drive> {
    socket: Pin<&'a mut TcpListener<D>>,
}

impl<'a, D: Drive> Accept<'a, D> {
}

impl<'a, D: Drive + Clone> Future for Accept<'a, D> {
    type Output = io::Result<(TcpStream<D>, SocketAddr)>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        self.socket.as_mut().poll_accept(ctx)
    }
}

pub struct Incoming<'a, D: Drive> {
    accept: Accept<'a, D>,
}

impl<'a, D: Drive> Incoming<'a, D> {
    fn inner(self: Pin<&mut Self>) -> Pin<&mut Accept<'a, D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.accept) }
    }
}

impl<'a, D: Drive + Clone> Stream for Incoming<'a, D> {
    type Item = io::Result<(TcpStream<D>, SocketAddr)>;

    fn poll_next(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = ready!(self.inner().poll(ctx));
        Poll::Ready(Some(next))
    }
}


pub struct Close<'a, D: Drive> {
    socket: Pin<&'a mut TcpListener<D>>,
}

impl<'a, D: Drive> Future for Close<'a, D> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket.as_mut().guard_op(Op::Close);
        let fd = self.socket.fd;
        ready!(self.socket.as_mut().ring().poll(ctx, true, 1, |sqs| unsafe {
            let mut sqe = sqs.single().unwrap();
            sqe.prep_close(fd);
            sqe
        }))?;
        Poll::Ready(Ok(()))
    }
}
