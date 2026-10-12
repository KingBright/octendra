mod config;
mod native;
use remote_hosts_desktop_protocol::{
    ErrorCode, RawPixels as PixelInfo, WorkerReply as Output, WorkerRequest as Envelope,
};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::{io, path::Path};
use tokio::io::unix::AsyncFd;
const MAX_REQUEST: usize = 64 * 1024;
// AsyncFd performs nonblocking pipe IO on owned duplicate FDs. Cancellation never
// leaves Tokio's blocking stdin/stdout task holding runtime shutdown open.
fn pipe_fd(fd: i32) -> io::Result<AsyncFd<std::fs::File>> {
    let duplicate = unsafe { nix::libc::fcntl(fd, nix::libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(duplicate) };
    let flags = unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_GETFL) };
    if flags < 0
        || unsafe {
            nix::libc::fcntl(
                file.as_raw_fd(),
                nix::libc::F_SETFL,
                flags | nix::libc::O_NONBLOCK,
            )
        } < 0
    {
        return Err(io::Error::last_os_error());
    }
    AsyncFd::new(file)
}
async fn read_exact(input: &AsyncFd<std::fs::File>, mut buffer: &mut [u8]) -> io::Result<()> {
    while !buffer.is_empty() {
        let mut ready = input.readable().await?;
        match ready.try_io(|fd| {
            let mut f = fd.get_ref();
            f.read(buffer)
        }) {
            Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(Ok(n)) => buffer = &mut buffer[n..],
            Ok(Err(e)) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(())
}
async fn write_all(output: &AsyncFd<std::fs::File>, mut buffer: &[u8]) -> io::Result<()> {
    while !buffer.is_empty() {
        let mut ready = output.writable().await?;
        match ready.try_io(|fd| {
            let mut f = fd.get_ref();
            f.write(buffer)
        }) {
            Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(Ok(n)) => buffer = &buffer[n..],
            Ok(Err(e)) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(())
}
async fn read_request(input: &AsyncFd<std::fs::File>) -> io::Result<Option<Envelope>> {
    let mut length = [0u8; 4];
    match read_exact(input, &mut length).await {
        Ok(_) => (),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_be_bytes(length) as usize;
    if n == 0 || n > MAX_REQUEST {
        return Err(io::Error::other("request bound"));
    }
    let mut data = vec![0; n];
    read_exact(input, &mut data).await?;
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|_| io::Error::other("request schema"))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--capabilities" {
        println!(
            "{}",
            serde_json::to_string(&remote_hosts_desktop_protocol::Capabilities::default())?
        );
        return Ok(());
    }
    let approval = match args.as_slice() {
        [_] => None,
        [_, flag, path] if flag == "--approval" => {
            Some(config::Approval::load(Path::new(path)).map_err(io::Error::other)?)
        }
        _ => {
            return Err(io::Error::other(
                "usage: --capabilities | --approval <0600 file> | disabled stdio",
            )
            .into());
        }
    };
    let mut worker = native::Worker::new(approval);
    let input = pipe_fd(0)?;
    let output = pipe_fd(1)?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let envelope = tokio::select! {
            biased;
            _=term.recv()=>break,
            _=tokio::signal::ctrl_c()=>break,
            result=tokio::time::timeout(worker.wait_budget(),read_request(&input))=>{
                match result { Ok(Ok(Some(e)))=>e, _=>break }
            }
        };
        if !seen.insert(envelope.request_id) || seen.len() > 128 {
            break;
        }
        // A cancelled/pending operation receives no fabricated success. The Agent's
        // existing journal observes EOF as unknown and never replays the same job.
        let result = tokio::select! {
            biased; _=term.recv()=>break, _=tokio::signal::ctrl_c()=>break,
            result=tokio::time::timeout(std::time::Duration::from_secs(25),worker.handle(envelope.request))=>{
                match result { Ok(r)=>r, Err(_)=>{let _=worker.shutdown().await; Err(ErrorCode::OutcomeUnknown)} }
            }
        };
        let mut frame = None;
        let (response, pixels) = match result {
            Ok(reply) => {
                let info = reply.pixels.as_ref().map(|f| PixelInfo {
                    encoding: match f.source_format.pixel_format {
                        host::linux_frame::PixelFormat::Nv12 => "nv12",
                        _ => "i420",
                    }
                    .into(),
                    size: [f.width, f.height],
                    color: [
                        f.source_format.color.range,
                        f.source_format.color.matrix,
                        f.source_format.color.transfer,
                        f.source_format.color.primaries,
                    ],
                    plane_lengths: f.planes.iter().map(Vec::len).collect(),
                    plane_sha256: native::plane_hashes(f),
                });
                frame = reply.pixels;
                (Ok(reply.response), info)
            }
            Err(e) => (Err(e), None),
        };
        let data = serde_json::to_vec(&Output {
            request_id: envelope.request_id,
            result: response,
            pixels,
        })?;
        let written = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            write_all(&output, &(data.len() as u32).to_be_bytes()).await?;
            write_all(&output, &data).await?;
            if let Some(f) = frame {
                for plane in &f.planes {
                    write_all(&output, plane).await?;
                }
            }
            Ok::<(), io::Error>(())
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            break;
        }
    }
    worker
        .shutdown()
        .await
        .map_err(|_| io::Error::other("cleanup outcome unknown"))?;
    Ok(())
}

// Agent transport/journal tests run in remote-hosts-code, which owns its Store.
