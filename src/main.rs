#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

use corona::io::BlockingWrapper;
use corona::prelude::*;
use failure::Error;
use futures::unsync::oneshot;
use log::{debug, error, info, trace, warn};
use tokio::net::unix::UnixListener;
use tokio::io::AsyncRead;

macro_rules! catch {
    ($( $b: tt )*) => {
        (|| -> Result<_, Error> { Ok({ $( $b )* } ) })()
    };
}

mod player;

use self::player::{Cmd, Mode};

static CONN_NUM: AtomicUsize = AtomicUsize::new(0);
const FORBIDDEN_EXTS: &[&str] = &[
    "htm",
    "html",
    "jpg",
    "jpeg",
    "ini",
    "bmp",
    "db",
    "doc",
    "dtt",
    "gif",
    "listing",
    "m3u",
    "nfo",
    "out",
    "pls",
    "txt",
    "toc",
    "zip",
];

fn handle_cmd(cmd: &[u8], lines: impl Iterator<Item = Result<Vec<u8>, io::Error>>)
    -> Result<bool, Error>
{
    let mut split = cmd.split(|c| *c == b' ')
        .filter(|word| !word.is_empty());
    if let Some(cmd) = split.next() {
        match cmd {
            b"mode" => {
                let mode = match split.next() {
                    Some(b"random") => Mode::Random,
                    Some(b"sequence") => Mode::Sequence,
                    Some(b"circular") => Mode::Circular,
                    Some(unknown) => {
                        error!("Unknown mode {}", String::from_utf8_lossy(unknown));
                        return Ok(true);
                    }
                    None => {
                        error!("Missing mode");
                        return Ok(true);
                    }
                };
                player::send(Cmd::Mode(mode));
            },
            b"load" => {
                let flags = split.collect::<HashSet<_>>();
                let append = flags.contains(b"append" as &[_]);
                // Go until you find the first empty line
                let mut songs = Vec::new();
                for line in lines {
                    let line = line?;
                    if line.is_empty() {
                        // End of block
                        break;
                    }

                    let path = PathBuf::from(OsString::from_vec(line));

                    if !path.is_file() {
                        warn!("Non-file {} in list of songs", path.to_string_lossy());
                        continue;
                    }
                    let forbidden = path.extension()
                        .and_then(OsStr::to_str)
                        .map(|ext| {
                            FORBIDDEN_EXTS
                                .iter()
                                .find(|forbidden| forbidden.eq_ignore_ascii_case(ext))
                                .is_some()
                        }).unwrap_or(false);
                    if forbidden {
                        trace!("Skipping forbidden file {}", path.to_string_lossy());
                        continue;
                    }

                    songs.push(path);
                }
                player::send(Cmd::Load { append, songs });
            }
            b"quit" => return Ok(false),
            b"terminate" => {
                player::send(Cmd::Stop);
                let (sender, receiver) = oneshot::channel();
                player::send(Cmd::Confirm(sender));
                let _ = receiver.coro_wait();
                process::exit(0);
            }
            b"play" => player::send(Cmd::Play),
            b"next" => player::send(Cmd::Next),
            b"prev" => player::send(Cmd::Prev),
            b"stop" => player::send(Cmd::Stop),
            _ => error!("Unknown command {}", String::from_utf8_lossy(cmd)),
        }
    } // Else → empty command, ignore
    Ok(true)
}

fn handle_conn(conn: impl AsyncRead) {
    let num = CONN_NUM.fetch_add(1, Ordering::Relaxed);
    info!("Accepted a control connection #{}", num);
    let mut lines = BufReader::new(BlockingWrapper::new(conn)).split(b'\n');
    let result = catch! {
        loop {
            let line = lines.next();
            match line {
                None => {
                    info!("Connection closed #{}", num);
                    break;
                }
                Some(cmd) => if !handle_cmd(&cmd?, &mut lines)? {
                    info!("Closing connection #{}", num);
                    break;
                },
            }
        }
    };
    if let Err(e) = result {
        error!("Error on connection #{}: {}", num, e);
    }
}

fn main() {
    env_logger::init();
    let result = Coroutine::new()
        .stack_size(65_536)
        .run(|| -> Result<(), Error> {
            // TODO: Configure
            // TODO: Signals
            let listener = UnixListener::bind("/home/vorner/.clue_play_socket")?;
            debug!("Created listening socket");
            for socket in listener.incoming().iter_result() {
                match socket {
                    Ok(socket) => {
                        corona::spawn(move || handle_conn(socket));
                    }
                    Err(err) => error!("Failed to accept connection: {}", err),
                }
            }
            unreachable!()
        }).unwrap();

    if let Err(e) = result {
        error!("Top level error: {}", e);
        process::exit(1);
    }
}
