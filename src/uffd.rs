use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

use log::info;
use userfaultfd::{
    Error, Event, EventBuffer, FaultKind, FeatureFlags, IoctlFlags, ReadWrite, RegisterMode, Uffd,
    UffdBuilder,
};

use crate::mmap::{Mmap, PageState};

const PAGE_SZ_4K: usize = 4 * 1024;
const PAGE_SZ_2M: usize = 2 * 1024 * 1024;
const HUGE_ZERO_PAGE: &[u8; PAGE_SZ_2M] = &[0u8; PAGE_SZ_2M];
const COPY_42_PAGE: &[u8; PAGE_SZ_2M] = &[0x42u8; PAGE_SZ_2M];

fn handle_zeropage(uffd: &Uffd, mmap: &Mmap, fault_addr: *mut c_void) {
    if mmap.page_size == PAGE_SZ_4K {
        unsafe {
            uffd.zeropage(fault_addr, PAGE_SZ_4K, false).unwrap();
            uffd.write_protect(fault_addr, PAGE_SZ_4K).unwrap();
            uffd.wake(fault_addr, PAGE_SZ_4K).unwrap();
        }
    } else {
        handle_copy(uffd, mmap, HUGE_ZERO_PAGE, fault_addr, true);
    }
}

fn handle_copy(uffd: &Uffd, mmap: &Mmap, data: &[u8], fault_addr: *mut c_void, wp: bool) {
    info!("Handling Missing fault: copying page");
    if let Err(err) = unsafe {
        uffd.copy(
            data.as_ptr().cast::<_>(),
            fault_addr,
            mmap.page_size,
            true,
            wp,
        )
    } {
        match err {
            Error::PartiallyCopied(bytes_copied) => {
                if bytes_copied == (-libc::EAGAIN) as usize {
                    panic!("Got a PartialCopy error with -EAGAIN");
                } else {
                    panic!("Got a PartialCopy error. Total bytes copied: {bytes_copied} bytes.");
                }
            }
            Error::CopyFailed(errno)
                if std::io::Error::from(errno).raw_os_error().unwrap() == libc::EEXIST => {}
            e => {
                panic!("Uffd copy failed: {e:?}");
            }
        }
    }
}

fn uffd_handler(uffd_raw: i32, mmap: Arc<Mmap>, tx: Sender<Message>, rx: Receiver<Message>) {
    let uffd = unsafe { Uffd::from_raw_fd(uffd_raw) };
    let mut events = EventBuffer::new(100);

    let mut deferred_events = vec![];
    let mut pages = vec![PageState::None; mmap.len / mmap.page_size];

    loop {
        // Wait for Continue command
        info!("Freezing now...");
        let cmd = rx.recv().unwrap();
        match cmd {
            Message::Continue => (),
            Message::Exit => return,
            cmd => panic!("uffd handler: unexpected command: '{cmd:?}'"),
        }
        info!("Thawed");

        for event in uffd.read_events(&mut events).unwrap() {
            deferred_events.push(event.unwrap());
        }

        info!("Found {} events in queue", deferred_events.len());

        for event in &deferred_events {
            match event {
                Event::Remove { start, end } => {
                    let start = *start;
                    let end = *end;
                    let page_idx = mmap.page_idx(start);
                    assert_eq!(unsafe { end.offset_from_unsigned(start) }, mmap.page_size);

                    info!("UFFD_EVENT_REMOVE: page: {page_idx} start={start:?}, end={end:?}");

                    if pages[page_idx] == PageState::Removed {
                        info!("Page already removed. Continuing...");
                        continue;
                    }

                    pages[page_idx] = PageState::Removed;
                }
                _ => continue,
            }
        }

        for event in &deferred_events {
            match event {
                Event::Pagefault { kind, rw, addr } => {
                    let addr = *addr;
                    assert!((addr as usize).is_multiple_of(mmap.page_size));
                    let page_idx = mmap.page_idx(addr);
                    info!(
                        "UFFD_EVENT_PAGEFAULT: page: {page_idx} kind={kind:?}, rw={rw:?}, addr={addr:?}"
                    );

                    match kind {
                        FaultKind::Missing => match pages[page_idx] {
                            PageState::Removed => {
                                info!("Page was previously removed. Providing zero page");
                                handle_zeropage(&uffd, &mmap, addr);
                                pages[page_idx] = PageState::Faulted;
                            }
                            PageState::Faulted => {
                                info!("Page already faulted. Nothing to do");
                            }
                            PageState::None => {
                                handle_copy(
                                    &uffd,
                                    &mmap,
                                    COPY_42_PAGE,
                                    addr,
                                    *rw == ReadWrite::Read,
                                );
                            }
                        },
                        FaultKind::WriteProtected => {
                            unreachable!("We don't handle WriteProtect faults currently");
                        }
                        FaultKind::Minor => panic!("Unexpected MINOR fault"),
                    }

                    pages[page_idx] = PageState::Faulted;
                }
                _ => continue,
            }
        }

        deferred_events.clear();
        tx.send(Message::Done).unwrap();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Message {
    Continue,
    Done,
    Exit,
}

pub struct UffdManager {
    uffd_features: FeatureFlags,
    uffd_ioctls: IoctlFlags,
    uffd_mode: RegisterMode,
    mmap: Arc<Mmap>,
}

impl UffdManager {
    pub fn new(mmap: Arc<Mmap>) -> Self {
        let uffd_features =
            FeatureFlags::EVENT_REMOVE | FeatureFlags::MISSING_HUGETLBFS | FeatureFlags::WP_ASYNC;
        let uffd_ioctls = IoctlFlags::REGISTER | IoctlFlags::UNREGISTER;
        let uffd_mode = RegisterMode::MISSING | RegisterMode::WRITE_PROTECT;

        UffdManager {
            uffd_features,
            uffd_ioctls,
            uffd_mode,
            mmap,
        }
    }

    pub fn start(&mut self) -> UffdHandler {
        let uffd = UffdBuilder::new()
            .user_mode_only(false)
            .non_blocking(true)
            .close_on_exec(true)
            .require_features(self.uffd_features)
            .require_ioctls(self.uffd_ioctls)
            .create()
            .unwrap();

        info!(
            "ACKed UFFD features: {:?} (bits: {:#x})",
            self.uffd_features,
            self.uffd_features.bits()
        );

        uffd.register_with_mode(self.mmap.addr, self.mmap.len, self.uffd_mode)
            .expect("Failed to register memory");
        info!(
            "Registered memory region: {:p} with length {}",
            self.mmap.addr, self.mmap.len
        );

        if self.mmap.page_size == PAGE_SZ_2M {
            info!("Working with huge pages. Can mark region as write-protected");
            uffd.write_protect(self.mmap.addr, self.mmap.len).unwrap();
        }

        let mmap = self.mmap.clone();
        let uffd_raw = uffd.as_raw_fd();
        let (tx_main, rx_handler) = std::sync::mpsc::channel();
        let (tx_handler, rx_main) = std::sync::mpsc::channel();
        let handler = std::thread::Builder::new()
            .name("uffd".to_string())
            .spawn(move || uffd_handler(uffd_raw, mmap, tx_handler, rx_handler))
            .expect("Could not spawn UFFD handler thread");
        info!("Handler thread spawned");

        UffdHandler {
            _uffd: uffd,
            handler,
            tx: tx_main,
            rx: rx_main,
        }
    }
}

pub struct UffdHandler {
    _uffd: Uffd,
    handler: JoinHandle<()>,
    tx: Sender<Message>,
    rx: Receiver<Message>,
}

impl UffdHandler {
    pub fn thaw(&self) {
        info!("Thawing UFFD handler");
        self.tx.send(Message::Continue).unwrap();
    }

    pub fn stop(self) {
        info!("Stopping UFFD handler");
        self.tx.send(Message::Exit).unwrap();
        self.handler.join().unwrap();
    }

    pub fn wait(&self) {
        info!("Waiting UFFD handler");
        // This should block until the handler sends us something
        let resp = self.rx.recv().unwrap();
        assert_eq!(resp, Message::Done);
    }
}
