//! Bounded native-wire regression for PR40. No transport, inference or external side effects.
use devworks_core::protocol::provider::EndpointProfile;
use devworks_core::providers::decode::{DecodedFrame, DecodedOutcome, Decoder};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Allocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static TRACKING: AtomicBool = AtomicBool::new(false);
fn added(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if TRACKING.load(Ordering::Relaxed) {
        ALLOCATED.fetch_add(bytes, Ordering::Relaxed);
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe {
            System.dealloc(pointer, layout);
        }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            added(size);
        }
        result
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
fn measured<T>(run: impl FnOnce() -> T) -> (T, usize, usize) {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            TRACKING.store(false, Ordering::SeqCst);
        }
    }
    let before = LIVE.load(Ordering::SeqCst);
    ALLOCATED.store(0, Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    TRACKING.store(true, Ordering::SeqCst);
    let stop = Stop;
    let result = run();
    drop(stop);
    (
        result,
        ALLOCATED.load(Ordering::SeqCst),
        PEAK.load(Ordering::SeqCst).saturating_sub(before),
    )
}

const ITEM_FIELDS: &str = r#""type":"message", "opaque":{"amount":1.2300e+004,"integer":9007199254740993,"escaped":"\u0061"}, "content":[{"type":"output_text","text":"native text","annotations":[]}]"#;
fn item(index: usize) -> String {
    format!(r#"{{ "id":"msg_{index}", "role":"assistant", "status":"completed", {ITEM_FIELDS} }}"#)
}
fn body(count: usize) -> String {
    let mut raw = String::from(
        r#"{ "id":"native-response", "model":"native-model", "status":"completed", "opaque-envelope":{ "escaped":"\u0062", "amount":9.8700e-009 }, "output":["#,
    );
    for i in 0..count {
        if i != 0 {
            raw.push(',');
        }
        raw.push_str(&item(i));
    }
    raw.push_str("] }");
    raw
}
fn native(
    frames: &[DecodedFrame],
) -> impl Iterator<Item = &devworks_core::providers::decode::NativeObservation> {
    frames.iter().filter_map(|frame| match frame {
        DecodedFrame::Native { observation } => Some(observation),
        _ => None,
    })
}
fn decode(profile: EndpointProfile, raw: &str, streaming: bool) -> Vec<DecodedFrame> {
    let mut decoder = Decoder::new(profile);
    if streaming {
        let mut frames = decoder.consume_data_event(Some("response.completed"), raw);
        assert!(decoder.protocol_ended());
        frames.extend(decoder.finish());
        frames
    } else {
        decoder.consume_aggregate(raw.as_bytes())
    }
}
#[test]
fn aggregate_and_stream_terminal_many_item_retention_and_allocations_are_linear() {
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
    ] {
        for streaming in [false, true] {
            let mut small = None;
            // The 64-item case fails on the old source using <1 MiB, before the large case
            // is ever constructed/decoded. This regression can run RED without an OOM.
            for count in [64, 5_000] {
                let body = body(count);
                let wire = if streaming {
                    format!(
                        r#"{{ "type":"response.completed", "opaque-event":1.23400e+02, "response":{body} }}"#
                    )
                } else {
                    body.clone()
                };
                let (frames, allocated, peak) =
                    measured(|| decode(profile.clone(), &wire, streaming));
                assert!(matches!(
                    frames.last(),
                    Some(DecodedFrame::Terminal {
                        outcome: DecodedOutcome::Completed,
                        ..
                    })
                ));
                let raw_bytes: usize = native(&frames).map(|item| item.raw_json.len()).sum();
                assert!(
                    raw_bytes <= 3 * wire.len(),
                    "quadratic retention: count={count}, wire={}, raw={raw_bytes}",
                    wire.len()
                );
                assert_eq!(
                    native(&frames).filter(|item| item.raw_json == wire).count(),
                    1,
                    "one enclosing native observation"
                );
                let items: Vec<_> = native(&frames)
                    .filter(|item| item.kind == "message")
                    .collect();
                assert_eq!(items.len(), count);
                for (i, item) in items.iter().enumerate() {
                    assert_eq!(item.ordinal, Some(i as u64));
                    assert_eq!(
                        item.raw_json,
                        self::item(i),
                        "exact escapes, decimal lexemes and opaque fields"
                    );
                    assert!(item.continuation.is_none());
                }
                let text: Vec<_> = frames
                    .iter()
                    .filter_map(|frame| match frame {
                        DecodedFrame::TextDelta {
                            block_index,
                            content_index,
                            text,
                        } => Some((*block_index, *content_index, text.as_str())),
                        _ => None,
                    })
                    .collect();
                assert_eq!(text.len(), count);
                assert!(
                    text.iter()
                        .enumerate()
                        .all(|(i, item)| *item == (i as u64, 0, "native text"))
                );
                assert!(
                    allocated < wire.len() * 128,
                    "allocation budget: {allocated} for {} bytes",
                    wire.len()
                );
                assert!(
                    peak < wire.len() * 96,
                    "live allocation budget: {peak} for {} bytes",
                    wire.len()
                );
                eprintln!(
                    "Responses streaming={streaming} count={count} wireBytes={} retainedRaw={raw_bytes} allocated={allocated} peakLive={peak}",
                    wire.len()
                );
                if let Some((small_bytes, small_allocated)) = small {
                    assert!(
                        (allocated as u128) * (small_bytes as u128)
                            < 2 * (small_allocated as u128) * (wire.len() as u128),
                        "allocated bytes grow faster than the input"
                    );
                } else {
                    small = Some((wire.len(), allocated));
                }
            }
        }
    }
}
