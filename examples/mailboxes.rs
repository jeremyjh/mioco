extern crate mio;
extern crate mioco;

extern crate env_logger;

use std::str;
use std::net::SocketAddr;
use std::str::FromStr;
use std::io::{Read, Write};

use mio::tcp::{TcpSocket};
use mioco::*;

const DEFAULT_LISTEN_ADDR : &'static str = "127.0.0.1:5555";

fn listend_addr() -> SocketAddr {
    FromStr::from_str(DEFAULT_LISTEN_ADDR).unwrap()
}

struct Mailbox(i32);

impl Notified for Mailbox{
    fn notify(&mut self, handle: MiocoHandle, msg: Message) {
        let msg = msg.downcast::<String>();
        println!("Got notification:{:?}",msg)
    }
}


fn main() {
    env_logger::init().unwrap();

    mioco::start(move |mioco| {
        let addr = listend_addr();

        let sock = try!(TcpSocket::v4());
        try!(sock.bind(&addr));
        let sock = try!(sock.listen(1024));

        println!("Starting tcp echo server on {:?}", sock.local_addr().unwrap());
        let sock = mioco.wrap(sock);

        loop {
            let conn = try!(sock.accept());
            let mb = mioco.wrap_notified(Mailbox(0));
            let sender = mb.channel();
            let _ = sender.send(Box::new("yes yes ya'll".to_string()));
            println!("before wait");
            mb.wait_notify();
            println!("we waited?");

            mioco.spawn(move |mioco| {
                let mut conn = mioco.wrap(conn);

                let mut buf = [0u8; 1024 * 16];
                loop {
                    let size = try!(conn.read(&mut buf));
                    if size == 0 {
                        /* eof */
                        break;
                    }
                    let bytes = &mut buf[0..(size)];
                    if str::from_utf8(bytes).unwrap().contains("quit") {
                       println!("Quitting.");
                       std::process::exit(0);
                    }
                    try!(conn.write_all(bytes))
                }

                Ok(())
            })
        }
    });
}
