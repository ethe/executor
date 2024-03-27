use std::{
    fmt,
    future::{poll_fn, Future},
    io::{self, Seek, SeekFrom, Write},
    pin::Pin,
    task::{Context, Poll},
};

use async_task::Task;

use crate::{
    blocking,
    futures::{ready, AsyncRead, AsyncSeek, AsyncWrite},
};

enum State {
    Idle(Option<std::fs::File>),
    InRead {
        reader: Option<piper::Reader>,
        task: Task<(io::Result<()>, std::fs::File)>,
    },
    InWrite {
        writer: Option<piper::Writer>,
        task: Task<(io::Result<()>, std::fs::File)>,
    },
    InSeek {
        task: Task<(SeekFrom, io::Result<u64>, std::fs::File)>,
    },
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Idle(_) => write!(f, "Idle"),
            State::InRead { .. } => write!(f, "InRead"),
            State::InWrite { .. } => write!(f, "InWrite"),
            State::InSeek { .. } => write!(f, "InSeek"),
        }
    }
}

pub struct File {
    state: State,
}

impl<T> From<T> for File
where
    std::fs::File: From<T>,
{
    fn from(file: T) -> Self {
        Self {
            state: State::Idle(Some(file.into())),
        }
    }
}

impl File {
    fn poll_stop(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match &mut self.state {
                State::InRead { reader, task } => {
                    drop(reader.take());
                    let (res, io) = ready!(Pin::new(task).poll(cx));
                    self.state = State::Idle(Some(io));
                    res?;
                }
                State::InWrite { writer, task } => {
                    drop(writer.take());
                    let (res, io) = ready!(Pin::new(task).poll(cx));
                    self.state = State::Idle(Some(io));
                    res?;
                }
                State::InSeek { task } => {
                    let (_, res, file) = ready!(Pin::new(task).poll(cx));
                    self.state = State::Idle(Some(file));
                    res?;
                }
                State::Idle(_) => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl AsyncRead for File {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.state {
                State::Idle(file) => {
                    let mut file = file.take().expect("file must be existed in idle state");
                    let (reader, mut writer) = piper::pipe(8 * 1024 * 1024);
                    let task = blocking::Executor::spawn(async move {
                        loop {
                            match poll_fn(|cx| writer.poll_fill(cx, &mut file)).await {
                                Ok(0) => return (Ok(()), file),
                                Ok(_) => {}
                                Err(err) => return (Err(err), file),
                            }
                        }
                    });
                    self.state = State::InRead {
                        reader: Some(reader),
                        task,
                    };
                }
                State::InRead { reader, task } => {
                    let n = ready!(reader
                        .as_mut()
                        .expect("reader must be had")
                        .poll_drain(cx, buf))?;

                    if n == 0 {
                        let (res, io) = ready!(Pin::new(task).poll(cx));
                        self.state = State::Idle(Some(io));
                        res?;
                    }

                    return Poll::Ready(Ok(n));
                }
                _ => ready!(self.poll_stop(cx))?,
            }
        }
    }
}

impl AsyncWrite for File {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.state {
                State::Idle(file) => {
                    let mut file = file.take().expect("file must be existed in idle state");
                    let (mut reader, writer) = piper::pipe(8 * 1024 * 1024);
                    let task = blocking::Executor::spawn(async move {
                        loop {
                            match poll_fn(|cx| reader.poll_drain(cx, &mut file)).await {
                                Ok(0) => return (file.flush(), file),
                                Ok(_) => {}
                                Err(err) => {
                                    file.flush().ok();
                                    return (Err(err), file);
                                }
                            }
                        }
                    });
                    self.state = State::InWrite {
                        writer: Some(writer),
                        task,
                    };
                }
                State::InWrite { writer, .. } => {
                    return writer
                        .as_mut()
                        .expect("writer must be had")
                        .poll_fill(cx, buf)
                }
                _ => ready!(self.poll_stop(cx))?,
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match &mut self.state {
                State::Idle(_) => return Poll::Ready(Ok(())),
                State::InRead { .. } | State::InWrite { .. } | State::InSeek { .. } => {
                    ready!(self.poll_stop(cx))?;
                }
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(Pin::new(&mut self).poll_flush(cx))?;
        self.state = State::Idle(None);
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for File {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: std::io::SeekFrom,
    ) -> Poll<io::Result<u64>> {
        loop {
            match &mut self.state {
                State::Idle(file) => {
                    let mut file = file.take().expect("file must be existed in idle state");
                    let task = blocking::Executor::spawn(async move {
                        let res = file.seek(pos);
                        (pos, res, file)
                    });
                    self.state = State::InSeek { task };
                }
                State::InSeek { task } => {
                    let (original_pos, res, io) = ready!(Pin::new(task).poll(cx));
                    self.state = State::Idle(Some(io));
                    let current = res?;

                    if original_pos == pos {
                        return Poll::Ready(Ok(current));
                    }
                }
                _ => ready!(self.poll_stop(cx))?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempfile;

    use crate::{
        futures::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
        Executor,
    };

    #[test]
    fn open_read_and_write() {
        Executor::builder()
            .worker_num(1)
            .build()
            .unwrap()
            .block_on(async {
                let mut file = super::File::from(tempfile().unwrap());
                file.write_all(b"hello").await.unwrap();
                file.seek(std::io::SeekFrom::Start(0)).await.unwrap();
                let mut buf = [0; 5];
                file.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"hello");
                file.close().await.unwrap();
            });
    }
}
