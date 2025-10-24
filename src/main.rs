use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const TARGET_ADDR: &str = "127.0.0.1:3000";
const REQUEST_BYTES: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n";
const TEST_DURATION: Duration = Duration::from_secs(5);

struct Metrics {
    total_requests: u64,
    total_bytes: u64,
    total_latency: Duration,
    min_latency: Duration,
    max_latency: Duration,
    max_index: Option<u64>,
    samples: Vec<Duration>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            total_requests: 0,
            total_bytes: 0,
            total_latency: Duration::ZERO,
            min_latency: Duration::ZERO,
            max_latency: Duration::ZERO,
            max_index: None,
            samples: Vec::new(),
        }
    }

    fn record(&mut self, latency: Duration, bytes: usize) {
        self.total_requests += 1;
        self.total_bytes += bytes as u64;
        self.total_latency += latency;
        if self.min_latency.is_zero() || latency < self.min_latency {
            self.min_latency = latency;
        }
        if latency > self.max_latency {
            self.max_latency = latency;
            self.max_index = Some(self.total_requests - 1);
        }
        self.samples.push(latency);
    }

    fn average_latency(&self) -> Duration {
        if self.total_requests == 0 {
            Duration::ZERO
        } else {
            self.total_latency / self.total_requests as u32
        }
    }

    fn percentile_values(&self, percentiles: &[f64]) -> Option<Vec<Duration>> {
        if self.samples.is_empty() {
            return None;
        }

        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let len = sorted.len() as f64;

        Some(
            percentiles
                .iter()
                .map(|p| {
                    let clamped = p.clamp(0.0, 100.0);
                    let rank = (clamped / 100.0) * (len - 1.0);
                    let idx = rank.round() as usize;
                    sorted[idx.min(sorted.len() - 1)]
                })
                .collect(),
        )
    }
}

fn main() -> io::Result<()> {
    let mut stream = TcpStream::connect(TARGET_ADDR)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_nodelay(true)?;

    let mut metrics = Metrics::new();
    let mut scratch = Vec::with_capacity(1024);

    let start = Instant::now();
    while start.elapsed() < TEST_DURATION {
        let iter_start = Instant::now();
        if let Err(err) = stream.write_all(REQUEST_BYTES) {
            eprintln!("write error: {err}");
            break;
        }

        match read_response(&mut stream, &mut scratch) {
            Ok(bytes) => {
                let latency = iter_start.elapsed();
                metrics.record(latency, bytes);
            }
            Err(err) => {
                eprintln!("read error: {err}");
                break;
            }
        }
    }

    report(start.elapsed(), &metrics);
    Ok(())
}

fn read_response(stream: &mut TcpStream, scratch: &mut Vec<u8>) -> io::Result<usize> {
    scratch.clear();

    let mut header_end = None;
    let mut expected_total: Option<usize> = None;

    loop {
        let mut buf = [0u8; 1024];
        let read = stream.read(&mut buf)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        scratch.extend_from_slice(&buf[..read]);

        if header_end.is_none() {
            if let Some(pos) = find_header_end(scratch) {
                header_end = Some(pos);
            }
        }

        if header_end.is_some() && expected_total.is_none() {
            let end = header_end.unwrap();
            expected_total = parse_content_length(&scratch[..end]).map(|len| end + len);
            if expected_total.is_none() {
                expected_total = Some(scratch.len());
            }
        }

        if let Some(total) = expected_total {
            if scratch.len() >= total {
                return Ok(total);
            }
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn parse_content_length(header_bytes: &[u8]) -> Option<usize> {
    let header = std::str::from_utf8(header_bytes).ok()?;
    for line in header.lines() {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn report(runtime: Duration, metrics: &Metrics) {
    println!("Ran for {:.2?} against {}", runtime, TARGET_ADDR);

    if metrics.total_requests == 0 {
        println!("  No successful requests recorded.");
        return;
    }

    let req_per_sec = if runtime.is_zero() {
        0.0
    } else {
        metrics.total_requests as f64 / runtime.as_secs_f64()
    };
    println!(
        "  Requests: {} ({:.2} req/s)",
        metrics.total_requests, req_per_sec
    );
    println!("  Bytes read: {}", metrics.total_bytes);
    println!(
        "  Latency: avg {:.2?}  min {:.2?}  max {:.2?}",
        metrics.average_latency(),
        metrics.min_latency,
        metrics.max_latency
    );
    if let Some(first) = metrics.samples.first() {
        println!("  First latency: {:.2?}", first);
    }
    if let Some(idx) = metrics.max_index {
        println!("  Max latency index: {}", idx);
    }
    if let Some(pcts) = metrics.percentile_values(&[90.0, 95.0, 99.0]) {
        println!(
            "  Percentiles: p90 {:.2?}  p95 {:.2?}  p99 {:.2?}",
            pcts[0], pcts[1], pcts[2]
        );
    }
    print_histogram(&metrics.samples, 10);
}

fn print_histogram(samples: &[Duration], buckets: usize) {
    if samples.is_empty() || buckets == 0 {
        return;
    }

    let mut values: Vec<f64> = samples
        .iter()
        .map(|d| d.as_secs_f64() * 1_000_000.0)
        .collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let min = values.first().copied().unwrap();
    let max = values.last().copied().unwrap();

    println!("  Latency histogram (µs):");

    if (max - min).abs() < f64::EPSILON {
        println!(
            "    {:>8.2}µs - {:>8.2}µs | {}",
            min,
            max,
            "#".repeat(samples.len().min(40))
        );
        return;
    }

    let bucket_width = (max - min) / buckets as f64;
    let mut counts = vec![0usize; buckets];

    for value in values {
        let mut idx = ((value - min) / bucket_width).floor() as usize;
        if idx >= buckets {
            idx = buckets - 1;
        }
        counts[idx] += 1;
    }

    for (i, count) in counts.iter().enumerate() {
        let low = min + bucket_width * i as f64;
        let high = if i == buckets - 1 {
            max
        } else {
            min + bucket_width * (i + 1) as f64
        };
        println!(
            "    {:>8.2}µs - {:>8.2}µs | {}",
            low,
            high,
            count
        );
    }
}
