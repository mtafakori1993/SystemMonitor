# Architecture notes

## Threads and async

Audit of every execution path (all in `src/`):

| Thread | Created by | Lifetime | Blocks on | Cancellation |
|---|---|---|---|---|
| UI (main) | eframe | process | never blocks: only `try_recv`, short lock-free reads | closes the window |
| `sampler` | `monitor::start` | until the `Sampler` handle drops | `/proc`, `/sys` reads, interruptible sleep | `Stop` flag + condvar; joined on drop (test: stops in <2 s with a 10 s interval) |
| `worker` | `linux::spawn` | one job | D-Bus, `Command::output`, UDP/TCP with timeouts | result discarded if the UI is gone; panic is caught and reported as "task ended unexpectedly" |
| `dbus-rt` (1 thread) | `linux::rt()` | process | polls zbus I/O | process exit |
| bench threads | `dns::bench` fan-out | one benchmark | UDP with 1.5 s timeout | checks `linux::shutdown()` between probes |
| a few `std::thread::spawn` in `proxy.rs` | detached one-shot jobs (detect, apply, keyring) | one job | D-Bus / `Command` | result channel dropped on exit |
| `docker-stats` | `docker::live::Live::start` (first visit of the Docker page) | until `Live` drops | Engine API over the Unix socket (15 s timeouts), interruptible sleep | `Stop` flag; joined on drop; idles when no Docker page was shown for 6 s |
| `docker-events` | same | until `Live` drops | `/events` stream (blocking read) | `Stop` flag + `Canceller` shuts the socket down; joined on drop |
| `docker-logs`, `docker-exec` | Containers tab | while the log view / terminal is open | log stream / hijacked exec socket | socket shut down when the session is dropped |

Rules the code follows:

* **No blocking call on the UI thread** and none on the Tokio worker: the runtime executes only zbus I/O
  futures; everything that sleeps, spawns a process or waits on a socket runs on a plain `worker` thread.
* **One entry point into the runtime**: `linux::block_on`. It is only ever called from plain threads. If it
  is ever reached from inside the runtime, it moves the caller off the executor (`block_in_place`) instead
  of panicking.
* **No lock is held across I/O.** The sampler owns a private `Snapshot`, publishes an `Arc<Snapshot>` by
  swapping a pointer under a mutex, and readers only clone that `Arc`. (Before, the sampler held the shared
  mutex while reading `/proc`, which could stall a frame.) All lock acquisitions tolerate poisoning.
* **Config files** are protected by an advisory `flock` (see `store.rs`), so two threads or two processes
  cannot interleave a read-modify-write.
* **Shutdown**: dropping `App` sets the process-wide `linux::shutdown()` token; dropping its `Sampler`
  stops and joins the sampler thread.

## Is Tokio necessary?

Yes, for this zbus configuration, and it is cheap, so it stays:

* zbus is built with its `tokio` feature. With the default `async-io` backend zbus would instead start its
  own executor thread **per connection**, and this program opens a short-lived connection per operation. With
  Tokio there is exactly one runtime thread, shared by every call.
* The runtime is `multi_thread` with **one** worker. A `current_thread` runtime would save nothing (the
  worker thread already exists) and would stall zbus's background tasks whenever no thread is inside
  `block_on`; with several worker threads calling `block_on` concurrently that would be a latent hang.
* `tokio::time` is used for the NetworkManager "re-check after 700 ms" and the Secret Service prompt timeout.
* Only `rt-multi-thread` and `time` are enabled; no `net`, `io`, or `macros`.

Alternatives considered and rejected: `zbus::blocking` (hides an internal executor and adds a thread per
connection), a hand-rolled `futures::executor` (zbus's tokio feature expects a Tokio reactor). The sync
`block_on` bridge from worker threads is the simplest reliable shape.

## Persistence

See the header comment of `src/store.rs`. JSON files, written atomically, versioned, backed up before
migrations, validated on load, locked for concurrent writers. SQLite was not adopted: the data is a few
small documents written rarely; SQLite would add a C dependency (a problem for the static core build) with no
measurable benefit.

## Build flavors and the GUI split

`Cargo.toml` has a default feature `gui` (eframe/egui). Everything that does not draw — metrics samplers,
DNS/proxy logic, SMART, persistence, graphics detection, the CLI — compiles without it
(`--no-default-features`). UI code is behind `#[cfg(feature = "gui")]`.
