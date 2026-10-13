//! What the dashboard's lanes cost the burn-in: ns per transform on one thread, median of 201 paired batches, and GFLOP/s on every
//! thread with the 100 ms sampler, rounds interleaved.
//! Usage: `cargo run --release -p memburn --example burnbench -- [rounds] [seconds]` (default 20 rounds of 2 s).
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use memburn_core::cpu;
use memburn_core::fft::{BurnStats, Burner, Plan};
use memburn_core::tui::FftView;

fn whole(lanes: bool, secs: f64, plans: &[Plan]) -> f64 {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let isa = cpu::best_isa();
    let stats = if lanes { BurnStats::with_lanes(threads) } else { BurnStats::default() };
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for i in 0..threads {
            let (stats, stop) = (&stats, &stop);
            s.spawn(move || Burner::new(isa, plans, i as u64).run(stats, stop));
        }
        let t0 = Instant::now();
        let f0 = stats.flops.load(Ordering::Relaxed);
        let mut view = lanes.then(|| FftView::new(&stats, threads, 0));
        while t0.elapsed().as_secs_f64() < secs {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(v) = &mut view {
                v.sample(&stats, t0.elapsed().as_millis() as u64);
            }
        }
        let f = stats.flops.load(Ordering::Relaxed) - f0;
        let el = t0.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        f as f64 / el / 1e9
    })
}

/// ns per transform of `log2n` on this thread without and with lanes, and the median ratio of 201 paired batches.
fn micro(log2n: u32) -> (f64, f64, f64) {
    let isa = cpu::best_isa();
    let plans = [Plan::new(log2n)];
    let n = (2_000usize >> log2n.saturating_sub(10)).max(4);
    let stats = [BurnStats::default(), BurnStats::with_lanes(1)];
    let mut b = Burner::new(isa, &plans, 0);
    let mut ns = [Vec::new(), Vec::new()];
    for k in 0..201 {
        let order = if k % 2 == 0 { [0, 1] } else { [1, 0] };
        for m in order {
            let t = Instant::now();
            for _ in 0..n {
                b.step(&stats[m]);
            }
            ns[m].push(t.elapsed().as_secs_f64() * 1e9 / n as f64);
        }
    }
    let mut ratio: Vec<f64> = ns[0].iter().zip(&ns[1]).map(|(p, l)| l / p).collect();
    let r = median(&mut ratio);
    let [mut p, mut l] = ns;
    (median(&mut p), median(&mut l), r)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rounds: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(20);
    let secs: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(2.0);
    for log2n in [10, 16] {
        let (p, l, r) = micro(log2n);
        println!("2^{log2n} on one thread: plain {p:.0} ns, lanes {l:.0} ns; paired {:+.2} %", (r - 1.0) * 100.0);
    }
    if rounds == 0 {
        return;
    }
    let plans: Vec<Plan> = (10..=21).map(Plan::new).collect();
    whole(false, 1.0, &plans);
    let (mut plain, mut lanes) = (Vec::new(), Vec::new());
    for k in 0..rounds {
        let order = if k % 2 == 0 { [false, true] } else { [true, false] };
        for l in order {
            let g = whole(l, secs, &plans);
            if l { lanes.push(g) } else { plain.push(g) }
        }
        println!("round {k}: plain {:.2} GFLOP/s, lanes {:.2} GFLOP/s", plain[k], lanes[k]);
    }
    let mut ratio: Vec<f64> = plain.iter().zip(&lanes).map(|(p, l)| l / p).collect();
    let r = median(&mut ratio);
    let (p, l) = (median(&mut plain), median(&mut lanes));
    println!("median: plain {p:.2}, lanes {l:.2} GFLOP/s; paired lanes/plain {:+.2} %", (r - 1.0) * 100.0);
}
