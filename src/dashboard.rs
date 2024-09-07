use std::time::Duration;

use bon::builder;
use eyre::{Report, Result};
use futures::TryStreamExt;
use lazy_static::lazy_static;
use prometheus::{register_int_counter, register_int_gauge, IntCounter, IntGauge};
use ratatui::{backend::Backend as BackendTrait, layout::Position, widgets::Clear, Terminal};
use replace_with::replace_with_or_abort;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    runtime::Builder,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_util::io::ReaderStream;

use crate::{
    events::{Broadcast, Event, Input, Keypress, StringError},
    io::{backend::Backend, Writer},
    widget::{apex::Apex, Raw, Widget},
};

lazy_static! {
    static ref TOTAL_DASHBOARD_THREADS: IntCounter = register_int_counter!(
        "dashboards_threads_total",
        "Total number of dashboard threads"
    )
    .unwrap();
    static ref ACTIVE_DASHBOARD_THREADS: IntGauge = register_int_gauge!(
        "dashboards_threads_active",
        "Number of active dashboard threads"
    )
    .unwrap();
}

static FPS: u16 = 10;
pub static RENDER_INTERVAL: Duration = Duration::from_millis(1000 / FPS as u64);

#[builder]
pub struct Dashboard {
    client: kube::Client,
}

impl Dashboard {
    // This spins up:
    // - An tokio async thread on the current runtime to handle IO by consuming
    //   `stdin` and publishing `Event`s on a channel.
    // - A *standard* thread which runs a new thread_local runtime to run the main
    //   dashboard rendering loop.
    //
    // Neither of these threads are awaited on, the dashboard can be dropped and as
    // long as:
    // - `stdin` or `stout` have not hit EOF
    // - `rx` has not been closed
    // - a `Event::Shutdown` has not been sent
    // They will continue to run in the background.
    pub fn start<R>(&mut self, stdin: R, stdout: impl Writer) -> Result<UnboundedSender<Event>>
    where
        R: AsyncRead + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();

        let reader_tx = tx.clone();
        tokio::spawn(async move {
            let stream = ReaderStream::new(stdin);
            tokio::pin!(stream);

            loop {
                tokio::select! {
                    () = reader_tx.closed() => {
                        break;
                    }
                    Ok(Some(msg)) = stream.try_next() => {
                        reader_tx.send(msg.into())?;
                    }
                }
            }

            Ok::<(), Report>(())
        });

        let rt = Builder::new_current_thread().enable_all().build()?;
        let client = self.client.clone();

        std::thread::spawn(move || {
            TOTAL_DASHBOARD_THREADS.inc();
            ACTIVE_DASHBOARD_THREADS.inc();

            if let Err(err) = rt.block_on(run(client, rx, stdout)) {
                tracing::error!("Unhandled dashboard error: {err:?}");
            }

            ACTIVE_DASHBOARD_THREADS.dec();
        });

        Ok(tx)
    }
}

impl std::fmt::Debug for Dashboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dashboard").finish()
    }
}

#[derive(Debug)]
enum Mode {
    UI(Box<dyn Widget>),
    Raw(Box<dyn Raw>, Box<dyn Widget>),
}

impl Mode {
    fn raw(&mut self, raw: Box<dyn Raw>) {
        replace_with_or_abort(self, |self_| match self_ {
            Self::UI(previous) | Self::Raw(_, previous) => Self::Raw(raw, previous),
        });
    }

    #[allow(clippy::match_wildcard_for_single_variants)]
    fn ui(&mut self) {
        replace_with_or_abort(self, |self_| match self_ {
            Self::Raw(_, previous) => Self::UI(previous),
            _ => self_,
        });
    }
}

async fn run(
    client: kube::Client,
    mut rx: UnboundedReceiver<Event>,

    stdout: impl Writer,
) -> Result<()> {
    let mut interval = tokio::time::interval(RENDER_INTERVAL);
    // Because we pause the render loop while rendering a raw widget, the ticks can
    // really back up. While this wouldn't necessarily be a bad thing (just some
    // extra CPU), it causes `Handle.data()` to deadlock if called too quickly.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let (backend, window_size) = Backend::with_size(stdout.blocking());
    let mut term = Terminal::new(backend)?;

    // kube::Client ends up being cloned by ~every widget, it'd be nice to Arc<> it
    // so that there's not a bunch of copying. Unfortunately, the Api interface
    // doesn't like Arc<>.
    let mut state = Mode::UI(Box::new(Apex::new(client)));

    loop {
        // It is important that this doesn't go *too* fast. Repeatedly writing to the
        // channel causes a deadlock for some reason that I've been unable to decipher.
        let ev = tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else {
                    break;
                };

                ev
            }
            _ = interval.tick() => {
                Event::Render
            }
        };

        if let Event::Resize(area) = ev {
            let mut size = window_size.lock().unwrap();
            *size = area;
        }

        let result = match state {
            Mode::UI(ref mut widget) => draw_ui(widget, &mut term, &ev)?,
            Mode::Raw(ref mut raw_widget, ref mut current_widget) => {
                let raw_result =
                    draw_raw(raw_widget, &mut term, &mut rx, stdout.non_blocking()).await;

                let result = current_widget.dispatch(&Event::Finished(
                    raw_result.map_err(|e| StringError(format!("{e:?}"))),
                ))?;

                state.ui();

                result
            }
        };

        match result {
            Broadcast::Exited => {
                break;
            }
            Broadcast::Raw(widget) => {
                state.raw(widget);
            }
            _ => {}
        }
    }

    term.draw(|frame| {
        frame.render_widget(Clear, frame.area());
        frame.set_cursor_position(Position::new(0, 0));
    })?;

    stdout.shutdown("exiting...".to_string()).await?;

    Ok(())
}

fn draw_ui<W>(
    widget: &mut Box<dyn Widget>,
    term: &mut Terminal<Backend<W>>,
    ev: &Event,
) -> Result<Broadcast>
where
    W: std::io::Write + Send,
{
    let result = match ev {
        Event::Input(Input { key, .. }) => {
            if matches!(key, Keypress::EndOfText) {
                return Ok(Broadcast::Exited);
            }

            widget.dispatch(ev)
        }
        Event::Render => Ok(Broadcast::Ignored),
        _ => widget.dispatch(ev),
    };

    term.draw(|frame| {
        if let Err(err) = widget.draw(frame, frame.area()) {
            panic!("{err}");
        }
    })?;

    result
}

async fn draw_raw(
    raw_widget: &mut Box<dyn Raw>,
    term: &mut Terminal<impl BackendTrait>,
    input: &mut UnboundedReceiver<Event>,
    output: impl AsyncWrite + Unpin + Send + 'static,
) -> Result<()> {
    term.clear()?;
    term.reset_cursor()?;

    raw_widget.start(input, Box::pin(output)).await?;

    term.clear()?;

    Ok(())
}

trait ResetScreen {
    fn reset_cursor(&mut self) -> Result<()>;
}

impl<B> ResetScreen for Terminal<B>
where
    B: BackendTrait,
{
    fn reset_cursor(&mut self) -> Result<()> {
        self.draw(|frame| {
            frame.set_cursor_position(Position::new(0, 0));
        })?;

        Ok(())
    }
}
