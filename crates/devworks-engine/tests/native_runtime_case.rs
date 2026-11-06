//! The case permit owns admission through unwinding; no runtime, network or model execution.
#[path = "common/native_runtime_case.rs"]
mod budget;

#[test]
fn unwinding_returns_the_owned_permit_and_two_permits_can_overlap() {
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let failed = std::panic::catch_unwind(|| {
            let _case = budget::acquire();
            panic!("controlled case unwind");
        });
        let first = budget::acquire();
        let second = budget::acquire();
        sent.send(failed.is_err()).unwrap();
        drop((first, second));
        let exclusive = budget::acquire_exclusive();
        drop(exclusive);
        let returned = budget::acquire();
        drop(returned);
    });
    assert!(
        received
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("two owned permits become available after case unwinding")
    );
    worker.join().unwrap();
}
