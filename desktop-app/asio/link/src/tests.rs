use super::*;
use std::mem::offset_of;

const ALIVE: Duration = Duration::from_secs(5);

fn pair(ring: usize) -> (Server, Client) {
    let region = HeapRegion::new(region_bytes(ring));
    let sig = LocalSignals::default();
    let server = Server::create(Box::new(region.clone()), Box::new(sig.clone()), ring, 1);
    let client = Client::open(Box::new(region), Box::new(sig), 2, |_| true).unwrap();
    (server, client)
}

/// Everything read until the link has nothing more, formats included.
fn drain(s: &mut Server, chunk: usize) -> Vec<Read> {
    let mut out = Vec::new();
    let mut buf = vec![0.0; chunk];
    loop {
        match s.read(&mut buf) {
            Read::Empty => return out,
            Read::Samples(n) => out.push(Read::Samples(n)),
            f => out.push(f),
        }
    }
}

#[test]
fn the_header_is_the_same_on_every_architecture() {
    // x86 drivers and the x64 app map the same bytes: every field fixed-size,
    // every 8-byte one 8-aligned, no padding the compiler chose.
    assert_eq!(std::mem::size_of::<Header>(), 1232);
    assert_eq!(std::mem::size_of::<Slot>(), 40);
    assert_eq!(std::mem::size_of::<LateSlot>(), 24);
    assert_eq!(offset_of!(Header, ring_samples), 16);
    assert_eq!(offset_of!(Header, read), 56);
    assert_eq!(offset_of!(Header, write), 80);
    assert_eq!(offset_of!(Header, name), 96);
    assert_eq!(offset_of!(Header, slots), 160);
    // The first version's header ends there: what came after it is new,
    // zero in a link an older server made.
    assert_eq!(offset_of!(Header, even), 800);
    assert_eq!(offset_of!(Header, calls), 808);
    assert_eq!(offset_of!(Header, late_calls), 848);
}

#[test]
fn a_client_finds_no_server_in_empty_memory_and_refuses_another_version() {
    let region = HeapRegion::new(region_bytes(64));
    let sig = LocalSignals::default();
    let e = Client::open(Box::new(region.clone()), Box::new(sig.clone()), 2, |_| true).err();
    assert_eq!(e, Some(LinkError::NoServer));
    let _s = Server::create(Box::new(region.clone()), Box::new(sig.clone()), 64, 1);
    // SAFETY: the header's version field, as another build would write it.
    unsafe { (*(region.as_ptr() as *const Header)).version.store(VERSION + 1, Release) };
    let e = Client::open(Box::new(region), Box::new(sig), 2, |_| true).err();
    assert_eq!(e, Some(LinkError::Version(VERSION + 1)));
}

#[test]
fn one_player_at_a_time_and_a_dead_ones_link_is_taken_over() {
    let region = HeapRegion::new(region_bytes(64));
    let sig = LocalSignals::default();
    let s = Server::create(Box::new(region.clone()), Box::new(sig.clone()), 64, 1);
    let a = Client::open(Box::new(region.clone()), Box::new(sig.clone()), 10, |_| true).unwrap();
    a.set_name("foobar2000");
    let busy = Client::open(Box::new(region.clone()), Box::new(sig.clone()), 11, |_| true).err();
    assert_eq!(busy, Some(LinkError::Busy));
    assert_eq!(s.client().unwrap().name, "foobar2000");
    // Process 10 died without letting go.
    std::mem::forget(a);
    let b = Client::open(Box::new(region.clone()), Box::new(sig.clone()), 11, |pid| pid != 10).unwrap();
    assert_eq!(s.client().unwrap().pid, 11);
    // The same process may take its own link again.
    let c = Client::open(Box::new(region.clone()), Box::new(sig.clone()), 11, |_| true).unwrap();
    assert!(c.gen() > b.gen());
    // The older one letting go leaves the newer one's hold.
    drop(b);
    assert_eq!(s.client().unwrap().pid, 11);
    drop(c);
    assert_eq!(s.client(), None);
}

#[test]
fn samples_come_through_bit_for_bit_across_the_ring_s_wrap() {
    let (mut s, mut c) = pair(64);
    s.set_target(48);
    s.set_mode(Mode::Listening);
    c.begin(44_100.0, 2, 18);
    // Values an int32, a float32 and a float64 source give, and odd bits.
    let special = [
        i32::MIN as f64 / 2_147_483_648.0,
        i32::MAX as f64 / 2_147_483_648.0,
        -1.0 / 2_147_483_648.0,
        f32::MIN_POSITIVE as f64,
        0.1f32 as f64,
        0.1,
        -0.0,
        f64::from_bits(0x7ff8_0000_dead_beef),
    ];
    let sent: Vec<f64> = (0..1000).map(|i| if i % 9 == 0 { special[i / 9 % special.len()] } else { i as f64 * 1e-3 }).collect();
    let mut got = Vec::new();
    let mut buf = [0.0; 7];
    let mut next = 0;
    let mut told = 0;
    while got.len() < sent.len() {
        while next < sent.len() && c.room(10) {
            let n = 10.min(sent.len() - next);
            c.write_with(n, |i| sent[next + i]);
            next += n;
        }
        match s.read(&mut buf) {
            Read::Samples(n) => got.extend_from_slice(&buf[..n]),
            Read::Format(f) => {
                told += 1;
                assert_eq!((f.rate, f.channels, f.sample_type), (44_100.0, 2, 18));
            }
            Read::Empty => panic!("the client put in more than was read"),
        }
    }
    assert_eq!(told, 1);
    let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&got), bits(&sent));
}

#[test]
fn the_client_runs_ahead_of_the_reader_by_the_target_and_no_more() {
    let (mut s, mut c) = pair(256);
    s.set_target(100);
    s.set_mode(Mode::Listening);
    c.begin(48_000.0, 2, 18);
    let mut put = 0;
    while c.room(16) {
        c.write_with(16, |_| 0.5);
        put += 16;
    }
    assert_eq!(put, 96);
    assert!(!c.room(16));
    let mut buf = [0.0; 40];
    assert!(matches!(s.read(&mut buf), Read::Format(_)));
    assert_eq!(s.read(&mut buf), Read::Samples(40));
    assert!(c.room(16));
    // A target under one block still lets a block into an empty ring.
    s.set_target(4);
    while s.read(&mut buf) != Read::Empty {}
    assert!(c.room(16));
}

#[test]
fn a_rate_change_falls_between_samples() {
    let (mut s, mut c) = pair(64);
    s.set_mode(Mode::Listening);
    c.begin(44_100.0, 2, 18);
    c.write_with(6, |i| i as f64);
    c.begin(96_000.0, 2, 19);
    c.write_with(4, |i| 10.0 + i as f64);
    c.end();
    let r = drain(&mut s, 16);
    let fmt = |r: &Read| match r {
        Read::Format(f) => Some((f.rate, f.sample_type)),
        _ => None,
    };
    assert_eq!(r.len(), 4, "{r:?}");
    assert_eq!(fmt(&r[0]), Some((44_100.0, 18)));
    assert_eq!(r[1], Read::Samples(6));
    assert_eq!(fmt(&r[2]), Some((96_000.0, 19)));
    assert_eq!(r[3], Read::Samples(4));
    // A stop and a start in the same format is a new segment too: its
    // format is told again, nothing is lost or doubled.
    c.begin(96_000.0, 2, 19);
    c.write_with(3, |_| 1.0);
    let r = drain(&mut s, 16);
    assert!(matches!(r[0], Read::Format(_)));
    assert_eq!(r[1..], [Read::Samples(3)]);
}

#[test]
fn held_keeps_the_stream_and_released_lets_it_go() {
    let (mut s, mut c) = pair(64);
    // Released (as made): nothing read.
    c.begin(44_100.0, 2, 18);
    c.write_with(5, |_| 1.0);
    let mut buf = [0.0; 16];
    assert_eq!(s.read(&mut buf), Read::Empty);
    assert_eq!(c.mode(ALIVE), Mode::Released);
    // Listening starts at the newest sample, in the open segment.
    s.set_mode(Mode::Listening);
    c.write_with(3, |_| 2.0);
    assert!(matches!(s.read(&mut buf), Read::Format(_)));
    assert_eq!(s.read(&mut buf), Read::Samples(3));
    assert_eq!(buf[..3], [2.0; 3]);
    // Held: nothing read, nothing lost.
    s.set_mode(Mode::Held);
    assert_eq!(c.mode(ALIVE), Mode::Held);
    c.write_with(4, |_| 3.0);
    assert_eq!(s.read(&mut buf), Read::Empty);
    s.set_mode(Mode::Listening);
    assert_eq!(s.read(&mut buf), Read::Samples(4));
    assert_eq!(buf[..4], [3.0; 4]);
}

#[test]
fn a_server_that_stops_beating_counts_as_released() {
    let (mut s, mut c) = pair(64);
    s.set_mode(Mode::Listening);
    let alive = Duration::from_millis(40);
    s.beat();
    assert_eq!(c.mode(alive), Mode::Listening);
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(c.mode(alive), Mode::Released);
    s.beat();
    assert_eq!(c.mode(alive), Mode::Listening);
    // A server that goes away says so at once.
    drop(s);
    assert_eq!(c.mode(alive), Mode::Released);
}

#[test]
fn a_long_name_is_cut_where_a_character_ends() {
    let (s, c) = pair(64);
    let name = "ж".repeat(40); // 80 bytes
    c.set_name(&name);
    assert_eq!(s.client().unwrap().name, "ж".repeat(32));
}

#[test]
fn two_threads_pass_a_long_stream_without_a_gap() {
    let (mut s, mut c) = pair(4096);
    s.set_target(2048);
    s.set_mode(Mode::Listening);
    const TOTAL: usize = 200_000;
    let writer = std::thread::spawn(move || {
        c.begin(192_000.0, 2, 19);
        let mut next = 0usize;
        while next < TOTAL {
            if !c.room(512) {
                c.wait_room(Duration::from_millis(50));
                continue;
            }
            let n = 512.min(TOTAL - next);
            c.write_with(n, |i| (next + i) as f64);
            next += n;
        }
        c.end();
        c
    });
    let mut buf = vec![0.0; 300];
    let mut expect = 0usize;
    while expect < TOTAL {
        match s.read(&mut buf) {
            Read::Samples(n) => {
                for &v in &buf[..n] {
                    assert_eq!(v, expect as f64);
                    expect += 1;
                }
            }
            Read::Format(_) => {}
            Read::Empty => {
                s.wait_data(Duration::from_millis(50));
            }
        }
    }
    let _c = writer.join().unwrap();
    assert_eq!(s.read(&mut buf), Read::Empty);
}

#[cfg(windows)]
mod windows {
    use super::*;

    fn base(test: &str) -> String {
        format!("AuraEngine.AudioLink.test.{}.{test}", std::process::id())
    }

    #[test]
    fn the_named_link_carries_samples_and_outlives_its_server() {
        let b = base("named");
        assert_eq!(win::open_client(&b).err(), Some(LinkError::NoServer));
        let mut s = win::create_server(&b, 1 << 12).unwrap();
        let mut c = win::open_client(&b).unwrap();
        c.set_name("test host");
        s.set_mode(Mode::Listening);
        c.begin(44_100.0, 2, 18);
        c.write_with(8, |i| i as f64 * 0.25);
        let mut buf = [0.0; 16];
        assert!(s.wait_data(Duration::from_millis(200)));
        assert!(matches!(s.read(&mut buf), Read::Format(_)));
        assert_eq!(s.read(&mut buf), Read::Samples(8));
        assert_eq!(buf[7], 1.75);
        assert_eq!(s.client().unwrap().name, "test host");
        // Aura Player closes while the player still holds the link, and starts
        // again: its server is made over the same memory, the player's place
        // kept.
        drop(s);
        assert_eq!(c.mode(ALIVE), Mode::Released);
        let mut s = win::create_server(&b, 1 << 12).unwrap();
        s.set_mode(Mode::Listening);
        assert_eq!(c.mode(ALIVE), Mode::Listening);
        c.write_with(2, |_| 9.0);
        assert!(matches!(s.read(&mut buf), Read::Format(_)));
        assert_eq!(s.read(&mut buf), Read::Samples(2));
        assert_eq!(buf[..2], [9.0, 9.0]);
    }

    #[test]
    fn this_process_is_alive() {
        assert!(win::process_alive(std::process::id()));
    }
}

/// The client's account of its calls reaches the server: every call
/// counted, the late ones on their own (the newest LATE_SLOTS kept, the
/// older said lost), the longest call and the waits added up; a new client
/// starts it afresh. The server's word that its reader is even reaches the
/// client.
#[test]
fn the_client_s_account_of_its_calls_reaches_the_server() {
    let region = HeapRegion::new(region_bytes(1 << 12));
    let sig = LocalSignals::default();
    let s = Server::create(Box::new(region.clone()), Box::new(sig.clone()), 1 << 12, 1);
    let c = Client::open(Box::new(region.clone()), Box::new(sig.clone()), 2, |_| true).unwrap();
    assert_eq!(s.call_stats(), CallStats::default(), "nothing yet");
    assert!(!c.even());
    s.set_even(true);
    assert!(c.even(), "the reader is even");
    c.set_period(Duration::from_micros(42_667));
    c.set_clock(true, -120);
    c.count_call(0, 900, None);
    let late = |k: u32| LateCall { gap_us: 90_000 + k, waited_us: 50_000, before_us: 700, fill: 4_096 };
    c.count_call(48_000, 1_500, Some(late(0)));
    let (mut seen, mut got) = (0u64, Vec::new());
    assert_eq!(s.late_calls(&mut seen, &mut got), 0);
    assert_eq!((seen, got.clone()), (1, vec![late(0)]));
    let st = s.call_stats();
    assert_eq!(
        st,
        CallStats { period_us: 42_667, calls: 2, late: 1, waited_us: 48_000, longest_us: 1_500, clocked: true, clock_ppm: -120 }
    );
    for k in 1..=20 {
        c.count_call(0, 100, Some(late(k)));
    }
    got.clear();
    assert_eq!(s.late_calls(&mut seen, &mut got), 20 - LATE_SLOTS as u64, "the older ones are gone");
    assert_eq!(got.len(), LATE_SLOTS);
    assert_eq!((got[0], got[LATE_SLOTS - 1], seen), (late(21 - LATE_SLOTS as u32), late(20), 21));
    // Another player takes the link: its account from nothing.
    std::mem::forget(c);
    let d = Client::open(Box::new(region), Box::new(sig), 3, |_| false).unwrap();
    assert_eq!(s.call_stats(), CallStats::default());
    d.count_call(0, 10, Some(late(99)));
    got.clear();
    assert_eq!(s.late_calls(&mut seen, &mut got), 0);
    assert_eq!((got, seen), (vec![late(99)], 1), "a count below the one seen: a new client's, from its first");
}
