use crate::auth::Sasl;
use crate::connection_options::ConnectionOptions;
use crate::frame_buffer::FrameBuffer;
use crate::heartbeats::{Heartbeat, HeartbeatState};
use crate::serialize::{OutputBuffer, IntoAmqpClass};
use crate::{ErrorKind, Result};
use amq_protocol::frame::AMQPFrame;
use amq_protocol::protocol::connection::{AMQPMethod, Close, CloseOk};
use amq_protocol::protocol::{AMQPClass, AMQPHardError};
use failure::{Fail, ResultExt};
use log::{debug, error, info, trace, warn};
use mio::net::TcpStream;
use mio::{Events, Poll, PollOpt, Ready, Token};
use mio_extras::timer::Timer;
use std::io;
use std::time::Duration;

const MAX_MISSED_SERVER_HEARTBEATS: u32 = 2;

const STREAM: Token = Token(0);
const HEARTBEAT: Token = Token(1);

#[derive(Debug)]
enum ConnectionState {
    Start,
    Secure,
    Tune,
    Open,
    Steady,
    Closing(Close),
}

impl ConnectionState {
    fn is_closing(&self) -> bool {
        match self {
            ConnectionState::Closing(_) => true,
            _ => false,
        }
    }

    fn process<Auth: Sasl>(&mut self, inner: &mut Inner<Auth>, frame: AMQPFrame) -> Result<()> {
        match self {
            ConnectionState::Start => match frame {
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::Start(start))) => {
                    debug!("received handshake {:?}", start);
                    let start_ok = inner.options.make_start_ok(start)?;
                    debug!("sending handshake {:?}", start_ok);
                    inner.push_method(0, AMQPMethod::StartOk(start_ok))?;
                    *self = ConnectionState::Secure;
                    Ok(())
                }
                _ => Err(ErrorKind::HandshakeWrongServerFrame("start", frame))?,
            },
            ConnectionState::Secure => match &frame {
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::Secure(secure))) => {
                    // We currently only support PLAIN and EXTERNAL, neither of which
                    // need a secure/secure-ok
                    debug!("received handshake {:?}", secure);
                    Err(ErrorKind::SaslSecureNotSupported)?
                }
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::Tune(_))) => {
                    *self = ConnectionState::Tune;
                    self.process(inner, frame)
                }
                _ => Err(ErrorKind::HandshakeWrongServerFrame(
                    "secure or tune",
                    frame,
                ))?,
            },
            ConnectionState::Tune => match frame {
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::Tune(tune))) => {
                    debug!("received handshake {:?}", tune);

                    let tune_ok = inner.options.make_tune_ok(tune)?;
                    inner.start_heartbeats(tune_ok.heartbeat);

                    debug!("sending handshake {:?}", tune_ok);
                    inner.push_method(0, AMQPMethod::TuneOk(tune_ok))?;

                    let open = inner.options.make_open();
                    debug!("sending handshake {:?}", open);
                    inner.push_method(0, AMQPMethod::Open(open))?;

                    *self = ConnectionState::Open;
                    Ok(())
                }
                _ => Err(ErrorKind::HandshakeWrongServerFrame("tune", frame))?,
            },
            ConnectionState::Open => match frame {
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::OpenOk(open_ok))) => {
                    debug!("received handshake {:?}", open_ok);
                    *self = ConnectionState::Steady;
                    Ok(())
                }
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::Close(close))) => {
                    inner.set_server_close_req(close)?;
                    Ok(())
                }
                _ => Err(ErrorKind::HandshakeWrongServerFrame("open-ok", frame))?,
            },
            ConnectionState::Closing(close) => match frame {
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::CloseOk(_))) => Err(
                    ErrorKind::ClientClosedConnection(close.reply_code, close.reply_text.clone()),
                )?,
                _ => {
                    trace!("discarding frame (waiting for close-ok)");
                    Ok(())
                }
            },
            ConnectionState::Steady => match frame {
                AMQPFrame::Method(0, AMQPClass::Connection(AMQPMethod::Close(close))) => {
                    inner.set_server_close_req(close)?;
                    Ok(())
                }
                other => {
                    let text = format!("do not know how to handle frame {:?}", other);
                    error!("{} - closing connection", text);
                    Ok(inner.set_our_close_req(Close {
                        reply_code: AMQPHardError::NOTIMPLEMENTED.get_id(),
                        reply_text: text,
                        class_id: 0,
                        method_id: 0,
                    })?)
                }
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum HeartbeatKind {
    Rx,
    Tx,
}

#[derive(Default)]
struct HeartbeatTimers {
    timer: Timer<HeartbeatKind>,
    rx: Option<Heartbeat<HeartbeatKind>>,
    tx: Option<Heartbeat<HeartbeatKind>>,
}

impl HeartbeatTimers {
    fn record_rx_activity(&mut self) {
        if let Some(hb) = &mut self.rx {
            trace!("recording activity for rx heartbeat");
            hb.record_activity();
        }
    }

    fn record_tx_activity(&mut self) {
        if let Some(hb) = &mut self.tx {
            trace!("recording activity for tx heartbeat");
            hb.record_activity();
        }
    }

    fn start(&mut self, interval: Duration) {
        assert!(
            self.rx.is_none() && self.tx.is_none(),
            "heartbeat timer started multiple times"
        );
        self.rx = Some(Heartbeat::start(
            HeartbeatKind::Rx,
            interval * MAX_MISSED_SERVER_HEARTBEATS,
            &mut self.timer,
        ));
        self.tx = Some(Heartbeat::start(
            HeartbeatKind::Tx,
            interval,
            &mut self.timer,
        ));
    }

    fn fire_rx(&mut self) -> HeartbeatState {
        let rx = self
            .rx
            .as_mut()
            .expect("fire_rx called on empty rx heartbeat");
        rx.fire(&mut self.timer)
    }

    fn fire_tx(&mut self) -> HeartbeatState {
        let tx = self
            .tx
            .as_mut()
            .expect("fire_tx called on empty tx heartbeat");
        tx.fire(&mut self.timer)
    }
}

struct CloseRequest {
    close: Close,
    pos: usize,
}

struct Inner<Auth: Sasl> {
    // Buffer of data waiting to be written.
    outbuf: OutputBuffer,

    // If we're going to send a CloseOk, we should close the connection immediately afterwards. If
    // server_close_req is Some, then we should close the connection after writing
    // outbuf[..server_close_req.pos]. server_close_req.pos may be larger than the size of a CloseOk frame
    // since there may be other data queued up in outbuf before the CloseOk goes out.
    server_close_req: Option<CloseRequest>,

    // See comment above; same applies if we're sending the Close request, except
    // after we send it we should discard any frames except CloseOk.
    our_close_req: Option<CloseRequest>,

    options: ConnectionOptions<Auth>,
    heartbeats: HeartbeatTimers,
}

impl<Auth: Sasl> Inner<Auth> {
    fn new(options: ConnectionOptions<Auth>, heartbeats: HeartbeatTimers) -> Self {
        Inner {
            outbuf: OutputBuffer::new(),
            server_close_req: None,
            our_close_req: None,
            options,
            heartbeats,
        }
    }

    #[inline]
    fn has_data_to_write(&self) -> bool {
        !self.outbuf.is_empty()
    }

    fn set_server_close_req(&mut self, close: Close) -> Result<()> {
        info!(
            "received close request from server ({}: {})",
            close.reply_code, close.reply_text
        );
        // sanity check - if we get multiple close requests, only keep the first
        if self.server_close_req.is_some() {
            return Ok(());
        }
        self.outbuf.push_method(0, AMQPMethod::CloseOk(CloseOk {}))?;
        self.server_close_req = Some(CloseRequest {
            close,
            pos: self.outbuf.len(),
        });
        Ok(())
    }

    #[inline]
    fn start_heartbeats(&mut self, interval: u16) {
        if interval > 0 {
            debug!("starting heartbeat timers ({} sec)", interval);
            self.heartbeats.start(Duration::from_secs(interval as u64));
        }
    }

    #[inline]
    fn push_method<M: IntoAmqpClass>(&mut self, channel_id: u16, method: M) -> Result<()> {
        self.outbuf.push_method(channel_id, method)
    }

    #[inline]
    fn record_rx_activity(&mut self) {
        self.heartbeats.record_rx_activity();
    }

    fn read_from_stream<S: io::Read>(
        &mut self,
        state: &mut ConnectionState,
        stream: &mut S,
        frame_buffer: &mut FrameBuffer,
    ) -> Result<()> {
        let n = frame_buffer.read_from(stream, |frame| {
            trace!("read frame {:?}", frame);
            Ok(match frame {
                // Heartbeats can come at any time, so filter them out here.
                // Let ConnectionState handle all other frames.
                AMQPFrame::Heartbeat(0) => debug!("received heartbeat"),
                frame => state.process(self, frame)?,
            })
        })?;
        if n > 0 {
            self.record_rx_activity();
        }
        Ok(())
    }

    fn set_our_close_req(&mut self, close: Close) -> Result<()> {
        // sanity check - if we get multiple close requests, only keep the first
        if self.our_close_req.is_some() {
            return Ok(());
        }
        self.outbuf.push_method(0, AMQPMethod::Close(close.clone()))?;
        self.our_close_req = Some(CloseRequest {
            close,
            pos: self.outbuf.len(),
        });
        Ok(())
    }

    fn write_to_stream<S: io::Write>(
        &mut self,
        state: &mut ConnectionState,
        stream: &mut S,
    ) -> Result<()> {
        // if we've already sent a close and we're just waiting for close-ok,
        // we should not write any more data.
        if state.is_closing() {
            // probably unnecessary, but in theory we could start filling
            // up outbuf with data we're never going to send, so clear it out.
            self.outbuf.clear();
            return Ok(());
        }

        // Decide how much queued data we're going to try to write. Normally,
        // the answer is "all we can (until WouldBlock)", but if the we're
        // halfway through a close, we should only send up through the serialized
        // close-ok (if server initiated) or close (if we initiate).
        let mut len = self.outbuf.len();
        if let Some(req) = &self.server_close_req {
            len = usize::min(len, req.pos);
        }
        if let Some(req) = &self.our_close_req {
            len = usize::min(len, req.pos);
        }

        // Main write loop; breaks when we've either written `len` bytes or
        // we get a WouldBlock.
        let mut pos = 0;
        while pos < len {
            trace!("trying to write {} bytes", len - pos);
            let n = match stream.write(&self.outbuf[pos..]) {
                Ok(n) => {
                    self.heartbeats.record_tx_activity();
                    n
                }
                Err(err) => match err.kind() {
                    io::ErrorKind::WouldBlock => {
                        if let Some(server_close_req) = &mut self.server_close_req {
                            server_close_req.pos -= pos;
                        }
                        let _ = self.outbuf.drain_written(pos);
                        return Ok(());
                    }
                    _ => return Err(err.context(ErrorKind::Io))?,
                },
            };
            pos += n;
        }

        // bookkeeping for close-ok in response to server's close
        if let Some(server_close_req) = &self.server_close_req {
            if len == server_close_req.pos {
                info!("sent close-ok in response to server's close request; dropping connection");
                return Err(ErrorKind::ServerClosedConnection(
                    server_close_req.close.reply_code,
                    server_close_req.close.reply_text.clone(),
                ))?;
            }
        }

        // bookkeeping for close (we now wait for a close-ok)
        if let Some(our_close_req) = &self.our_close_req {
            if len == our_close_req.pos {
                info!("sent close request to server");
                *state = ConnectionState::Closing(our_close_req.close.clone());
            }
        }

        // Wrote everything we have - use clear instead of .drain_written(). If we just sent a
        // close request, there might be data leftover here, but go ahead and clear it anyway (see
        // comment at top of this method).
        // TODO see if more writes are incoming from clients first?
        self.outbuf.clear();
        Ok(())
    }

    fn process_heartbeat_timers(&mut self) -> Result<()> {
        while let Some(kind) = self.heartbeats.timer.poll() {
            match kind {
                HeartbeatKind::Rx => match self.heartbeats.fire_rx() {
                    HeartbeatState::StillRunning => {
                        trace!("rx heartbeat timer fired, but have received data since last");
                    }
                    HeartbeatState::Expired => {
                        error!("missed heartbeats from server - closing connection");
                        return Err(ErrorKind::MissedServerHeartbeats)?;
                    }
                },
                HeartbeatKind::Tx => match self.heartbeats.fire_tx() {
                    HeartbeatState::StillRunning => {
                        trace!("tx heartbeat timer fired, but have sent data since last");
                    }
                    HeartbeatState::Expired => {
                        // if we already have data queued up to send, don't bother also
                        // enqueuing up a heartbeat frame
                        if self.outbuf.is_empty() {
                            debug!("sending heartbeat");
                            self.outbuf.push_heartbeat();
                        } else {
                            warn!("tx heartbeat fired, but already have queued data to write - possible socket problem");
                        }
                    }
                },
            }
        }
        Ok(())
    }
}

pub struct EventLoop<Auth: Sasl> {
    stream: TcpStream,
    poll: Poll,
    frame_buffer: FrameBuffer,
    inner: Inner<Auth>,
    state: ConnectionState,
}

impl<Auth: Sasl> EventLoop<Auth> {
    pub fn new(options: ConnectionOptions<Auth>, stream: TcpStream) -> Result<Self> {
        let heartbeats = HeartbeatTimers::default();

        let poll = Poll::new().context(ErrorKind::Io)?;
        poll.register(
            &stream,
            STREAM,
            Ready::readable() | Ready::writable(),
            PollOpt::edge(),
        )
        .context(ErrorKind::Io)?;
        poll.register(
            &heartbeats.timer,
            HEARTBEAT,
            Ready::readable(),
            PollOpt::edge(),
        )
        .context(ErrorKind::Io)?;

        Ok(EventLoop {
            stream,
            poll,
            frame_buffer: FrameBuffer::new(),
            inner: Inner::new(options, heartbeats),
            state: ConnectionState::Start,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        match self.main_loop() {
            Ok(()) => Ok(()),
            Err(err) => match self.state {
                // if we send bad credentials, the socket gets dropped without
                // a close message, but we can tell clients it was an auth problem
                // if we had made it to that step in the handshake.
                ConnectionState::Secure => {
                    Err(err.context(ErrorKind::InvalidCredentials))?
                },
                _ => Err(err),
            }
        }
    }

    fn main_loop(&mut self) -> Result<()> {
        let mut events = Events::with_capacity(128);
        loop {
            self.poll
                .poll(&mut events, self.inner.options.poll_timeout)
                .context(ErrorKind::Io)?;
            if events.is_empty() {
                return Err(ErrorKind::SocketPollTimeout)?;
            }

            let had_data_to_write = self.inner.has_data_to_write();

            for event in events.iter() {
                match event.token() {
                    STREAM => {
                        if event.readiness().is_writable() {
                            self.inner
                                .write_to_stream(&mut self.state, &mut self.stream)?;
                        }
                        if event.readiness().is_readable() {
                            self.inner.read_from_stream(
                                &mut self.state,
                                &mut self.stream,
                                &mut self.frame_buffer,
                            )?;
                        }
                    }
                    HEARTBEAT => self.inner.process_heartbeat_timers()?,
                    _ => unreachable!(),
                }
            }

            let have_data_to_write = self.inner.has_data_to_write();

            // possibly change how we're registered to the socket:
            // 1. If we had data and now we don't, switch to readable only.
            // 2. If we didn't have data and now we do, switch to read+write.
            if had_data_to_write && !have_data_to_write {
                trace!("had queued data but now we don't - waiting for socket to be readable");
                self.poll
                    .reregister(&self.stream, STREAM, Ready::readable(), PollOpt::edge())
                    .context(ErrorKind::Io)?;
            } else if !had_data_to_write && have_data_to_write {
                trace!("didn't have queued data but now we do - waiting for socket to be readable or writable");
                self.poll
                    .reregister(
                        &self.stream,
                        STREAM,
                        Ready::readable() | Ready::writable(),
                        PollOpt::edge(),
                    )
                    .context(ErrorKind::Io)?;
            }
        }
    }
}