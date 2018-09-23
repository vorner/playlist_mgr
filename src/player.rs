use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Error as IoError, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt as UnixCommandExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use corona::io::BlockingWrapper;
use corona::prelude::*;
use failure::Error;
use futures::unsync::oneshot::Sender;
use futures::unsync::mpsc::{self, UnboundedSender as QueueSender};
use id3::Tag;
use log::{debug, error, info};
use nix::unistd;
use rand::Rng;
use tokio::reactor::Handle;
use tokio::net::unix::UnixStream;
use tokio_process::CommandExt;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Random,
    Sequence,
    Circular,
}

#[derive(Debug)]
pub(crate) enum Cmd {
    Play,
    Stop,
    Next,
    Prev,
    Load {
        songs: Vec<PathBuf>,
        append: bool,
    },
    Mode(Mode),
    Confirm(Sender<()>),
    Done,
}

struct Player {
    mode: Mode,
    songs: Vec<PathBuf>,
    history: VecDeque<PathBuf>,
    playlist: Vec<PathBuf>,
    current: Option<PathBuf>,
    should_play: bool,
    position: usize,
    control_pipe: Option<BlockingWrapper<UnixStream>>,
    last_start: Option<Instant>,
}

impl Player {
    fn new() -> Self {
        Player {
            mode: Mode::Random,
            songs: Vec::new(),
            history: VecDeque::new(),
            playlist: Vec::new(),
            current: None,
            should_play: false,
            position: 0,
            control_pipe: None,
            last_start: None,
        }
    }

    fn done(&mut self) {
        if let Some(current) = self.current.take() {
            self.history.push_back(current);
            while self.history.len() > 100 {
                self.history.pop_front();
            }
        }

        self.control_pipe = None;
        self.last_start = None;

        if self.should_play {
            self.start();
        }
    }

    fn choose_song(&mut self) -> Option<PathBuf> {
        if let Some(song) = self.playlist.pop() {
            return Some(song);
        }

        if self.songs.is_empty() {
            return None;
        }

        match self.mode {
            Mode::Random => self.position = rand::thread_rng().gen_range(0, self.songs.len()),
            Mode::Sequence if self.position > self.songs.len() => self.position = 0,
            Mode::Circular if self.position >= self.songs.len() => self.position = 0,
            _ => (),
        }

        let next = self.songs.get(self.position).cloned();

        self.position += 1;

        next
    }


    fn start(&mut self) {
        if let Some(song) = self.choose_song() {
            assert!(self.control_pipe.is_none());
            assert!(self.current.is_none());

            let child = catch! {
                debug!("Starting mpv with {}", song.to_string_lossy());

                let (sender, receiver) = StdUnixStream::pair()?;

                let receiver_fd = receiver.as_raw_fd();

                let child = Command::new("/usr/bin/mpv")
                    .args(&["-really-quiet", "-vo", "null", "--input-file=fd://4"])
                    .arg(&song)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .before_exec(move || {
                        unistd::dup2(receiver_fd, 4)
                            .map(|_| ())
                            .map_err(|_| IoError::last_os_error())
                    }).spawn_async()?;

                let sender = UnixStream::from_std(sender, &Handle::default())?;
                (child, sender)
            };

            let info = Tag::read_from_path(&song)
                .map(|tag| {
                    let album = tag.album().unwrap_or("???");
                    let artist = tag.artist().unwrap_or("???");
                    let title = tag.title().unwrap_or("???");
                    format!("{} ({}/{})", title, artist, album)
                }).unwrap_or_else(|_| "???".to_owned());

            println!("• {}\n  {}", info, song.to_string_lossy());

            match child {
                Err(e) => {
                    error!("Failed to start mpv: {}", e);
                    self.should_play = false;
                }
                Ok((child, control)) => {
                    self.control_pipe = Some(BlockingWrapper::new(control));
                    self.current = Some(song);
                    self.last_start = Some(Instant::now());

                    corona::spawn(move || {
                        match child.coro_wait() {
                            Err(e) => error!("Error waiting for mpv: {}", e),
                            Ok(status) => if status.success() {
                                debug!("Terminated successfully");
                            } else {
                                error!("Mpv: {}", status);
                            }
                        }

                        send(Cmd::Done);
                    });
                }
            }
        } else {
            info!("Nothing to play");
            self.should_play = false;
        }
    }

    fn send_mpv(&mut self, key: &[u8]) {
        if let Some(control) = self.control_pipe.as_mut() {
            debug!("Sending command {}", String::from_utf8_lossy(key));
            // It might fail if the other end terminates, right?
            let _ = control.write_all(key);
        } else {
            debug!("Nowhere to send command {}", String::from_utf8_lossy(key));
        }
    }

    fn pause(&mut self) {
        self.send_mpv(b"pause\n");
    }

    fn play_pause(&mut self) {
        self.should_play = true;
        if self.control_pipe.is_some() {
            self.pause();
        } else {
            self.start();
        }
    }

    fn next(&mut self) {
        self.should_play = true;

        if self.control_pipe.is_some() {
            self.stop_song();
        } else {
            self.start();
        }
    }

    fn prev(&mut self) {
        if let Some(current) = self.current.take() {
            self.playlist.push(current);
        }

        // If the current song (we just moved above) played for long enough, restart it. If not,
        // place the previous song before it.
        let restart = self.last_start.take()
            .map(|last| Instant::now() - last > Duration::from_secs(2))
            .unwrap_or(false);

        if !restart {
            if let Some(prev) = self.history.pop_back() {
                self.playlist.push(prev);
            }
        }

        // Going forward instead of back if there's no back would be stupid
        if !self.playlist.is_empty() {
            // We just pick the next song from the playlist ‒ the one we just placed there.
            self.next();
        }
    }

    fn stop(&mut self) {
        self.should_play = false;
        self.stop_song();
    }

    fn stop_song(&mut self) {
        self.send_mpv(b"quit\n");
    }

    fn cmd(&mut self, cmd: Cmd) {
        use self::Cmd::*;

        debug!("Executing command {:#?}", cmd);

        match cmd {
            Play => self.play_pause(),
            Stop => self.stop(),
            Next => self.next(),
            Prev => self.prev(),
            Load { songs, append } => {
                if append {
                    self.songs.extend(songs);
                } else {
                    self.songs = songs;
                    self.position = 0;
                }
            }
            Mode(mode) => self.mode = mode,
            Confirm(sender) => {
                let _ = sender.send(());
            }
            Done => self.done(),
        }
    }
}

fn start_player() -> QueueSender<Cmd> {
    let (sender, receiver) = mpsc::unbounded();

    corona::spawn(move || {
        let mut player = Player::new();

        for cmd in receiver.iter_ok() {
            player.cmd(cmd);
        }
        unreachable!();
    });

    sender
}

thread_local! {
    // Thread local for a single-threaded application ‒ but rust otherwise insists on mutexes
    static QUEUE: RefCell<QueueSender<Cmd>> = RefCell::new(start_player());
}

pub(crate) fn send(cmd: Cmd) {
    let _ = QUEUE.with(|q| q.borrow_mut().unbounded_send(cmd));
}
