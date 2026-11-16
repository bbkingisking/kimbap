//! Run against local corpus without GTK initialization. The second pass tests
//! bounded cache reuse; it is not a decoder-only microbenchmark.
use std::{path::PathBuf, time::Duration};
fn main() {
    let mut args = std::env::args().skip(1);
    let worker = PathBuf::from(args.next().expect("worker executable, then JPEG paths"));
    let files: Vec<_> = args.map(PathBuf::from).collect();
    let (decoder, replies) = kimbap::jpeg::Decoder::new(worker);
    println!("round,file,width,height,decode_us,load_us,cache,worker_pid");
    let mut generation = 0;
    for round in 0..3 {
        for file in &files {
            generation += 1;
            decoder.load(generation, file.clone());
            let reply = replies.recv_blocking().expect("worker reply");
            assert_eq!(reply.generation, generation);
            match reply.result {
                Ok(image) => println!(
                    "{round},\"{}\",{},{},{},{},{},{}",
                    file.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .replace('"', "\"\""),
                    image.frame.width,
                    image.frame.height,
                    image.frame.decode_us,
                    image.elapsed.as_micros(),
                    image.cache_hit,
                    image.worker_pid.unwrap_or(0)
                ),
                Err(error) => panic!("{}: {error}", file.display()),
            }
        }
    }
    decoder.cancel(generation + 1);
    replies.close();
    drop(decoder);
    std::thread::sleep(Duration::from_millis(10));
}
