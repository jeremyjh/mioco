# mioco

<p align="center">
  <a href="https://travis-ci.org/dpc/mioco">
      <img src="https://img.shields.io/travis/dpc/mioco/master.svg?style=flat-square" alt="Build Status">
  </a>
  <a href="https://crates.io/crates/mioco">
      <img src="http://meritbadge.herokuapp.com/mioco?style=flat-square" alt="crates.io">
  </a>
  <a href="https://gitter.im/dpc/mioco">
      <img src="https://img.shields.io/badge/GITTER-join%20chat-green.svg?style=flat-square" alt="Gitter Chat">
  </a>
  <br>
  <strong><a href="//dpc.github.io/mioco/">Documentation</a></strong>
</p>


## Introduction

Scalable, asynchronous IO coroutine-based handling (aka MIO COroutines).

Using `mioco` you can handle scalable, asynchronous [`mio`][mio]-based IO, using set of synchronous-IO
handling functions. Based on asynchronous [`mio`][mio] events `mioco` will cooperatively schedule your
handlers.

You can think of `mioco` as of *Node.js for Rust* or *[green threads][green threads] on top of [`mio`][mio]*.

`mioco` is still very experimental, but already usable. For real-life project using
`mioco` see [colerr][colerr].

Read [Documentation](//dpc.github.io/mioco/) for details.

If you need help, try asking on [#mioco gitter.im][mioco gitter]. If still no
luck, try [rust user forum][rust user forum].

To report a bug or ask for features use [github issues][issues].

[rust]: http://rust-lang.org
[mio]: //github.com/carllerche/mio
[colerr]: //github.com/dpc/colerr
[mioco gitter]: https://gitter.im/dpc/mioco
[rust user forum]: https://users.rust-lang.org/
[issues]: //github.com/dpc/mioco/issues
[green threads]: https://en.wikipedia.org/wiki/Green_threads

## Building & running

Note: You must be using [nightly Rust][nightly rust] release. If you're using
[multirust][multirust], which is highly recommended, switch with `multirust default
nightly` command.

    cargo build --release
    make echo

[nightly rust]: https://doc.rust-lang.org/book/nightly-rust.html
[multirust]: https://github.com/brson/multirust

# Benchmarks

Beware: This is very naive comparison! I tried to run it fairly,
but I might have missed something. Also no effort was spent on optimizing
neither `mioco` nor other tested tcp echo implementations.

In thousands requests per second:

|         | `bench1` | `bench2` |
|:--------|---------:|---------:|
| `libev` | 183      | 225      |
| `node`  | 37       | 42       |
| `mio`   | TBD      | TBD      |
| `mioco` | 157      | 177      |


Server implementation tested:

* `libev` - https://github.com/dpc/benchmark-echo/blob/master/server_libev.c ;
   Note: this implementation "cheats", by waiting only for read events, which works
   in this particular scenario.
* `node` - https://github.com/dpc/node-tcp-echo-server ;
* `mio` - TBD. See: https://github.com/hjr3/mob/issues/1 ;
* `mioco` - https://github.com/dpc/mioco/blob/master/examples/echo.rs;

Benchmarks used:

* `bench1` - https://github.com/dpc/benchmark-echo ; `PARAMS='-t64 -c10 -e10000 -fdata.json'`;
* `bench2` - https://gist.github.com/dpc/8cacd3b6fa5273ffdcce ; `GOMAXPROCS=64 ./tcp_bench  -c=128 -t=30 -a=""`;

Machine used:

* i7-3770K CPU @ 3.50GHz, 32GB DDR3 1800Mhz, some basic overclocking, Fedora 21;

